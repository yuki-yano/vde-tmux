//! Bounded subprocess termination that terminates descendants and reaps the
//! group leader.
//!
//! A child spawned in its own process group can leave a descendant that
//! inherited its stdout/stderr; the reader threads then block until that
//! descendant exits. Killing only the group leader does not help, and killing
//! the group *after* reaping the leader risks signalling a reused pgid. These
//! helpers detect the leader's exit without reaping it (so the pgid stays
//! valid), kill the whole group (which terminates the descendants; init reaps
//! them), and only then reap the leader.

use std::io::{Error, ErrorKind, Result};
use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

/// Poll whether `pid` has terminated, without reaping it (`WNOWAIT`), so the
/// process — and therefore its process group id — stays valid for a following
/// group kill. `Ok(true)` once the child has exited, `Ok(false)` while it is
/// still running or when the call was interrupted (`EINTR`), `Err` on any other
/// `waitid` failure. `EINTR` is reported as "not yet exited" rather than retried
/// in place so the caller's deadline still governs the wait.
fn child_exited_without_reaping(pid: i32) -> Result<bool> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // WEXITED: report terminated children. WNOWAIT: leave it reapable.
    // WNOHANG: return immediately if it has not terminated yet.
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
        )
    };
    if rc == 0 {
        // With WNOHANG, si_signo stays 0 while the child is still running and is
        // set to SIGCHLD once it has terminated.
        return Ok(info.si_signo != 0);
    }
    let error = Error::last_os_error();
    if error.kind() == ErrorKind::Interrupted {
        return Ok(false);
    }
    Err(error)
}

/// Poll `check` until it reports exit (`Ok(true)`), the `deadline` passes
/// (`Ok(false)`), or it fails (`Err`). Bounded by `deadline` regardless of how
/// often `check` reports "not yet".
fn poll_until_exit_or_deadline(
    deadline: Instant,
    mut check: impl FnMut() -> Result<bool>,
) -> Result<bool> {
    loop {
        match check() {
            Ok(true) => return Ok(true),
            Ok(false) => {}
            Err(error) => return Err(error),
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// SIGKILL the whole process group led by `pgid`. Only call this while the
/// group leader still exists (alive or an unreaped zombie) so the pgid cannot
/// have been recycled.
fn kill_process_group(pgid: i32) {
    // Ignore the result: ESRCH just means the group is already gone.
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
}

/// Wait up to `timeout` for `child` (which must have been spawned in its own
/// process group) to exit on its own, then SIGKILL the whole group and reap the
/// leader. `Ok(Some(status))` if the child exited before the timeout,
/// `Ok(None)` if it had to be killed, `Err` on a `waitid`/`wait` failure.
///
/// The group kill happens before the leader is reaped, so any descendant that
/// inherited the child's pipes is terminated and the pgid cannot be reused. The
/// group is always killed and the leader always reaped, even on error.
pub fn await_exit_then_kill_group(
    child: &mut Child,
    timeout: Duration,
) -> Result<Option<ExitStatus>> {
    let pid = child.id() as i32;
    let outcome = poll_until_exit_or_deadline(Instant::now() + timeout, || {
        child_exited_without_reaping(pid)
    });
    // The leader is still present here (a zombie if it exited, alive on
    // timeout), so its pgid is valid.
    kill_process_group(pid);
    let reaped = child.wait();
    match outcome {
        // Surface the original waitid error, but only after reaping the leader.
        Err(error) => Err(error),
        Ok(exited) => Ok(exited.then_some(reaped?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    fn spawn_group(script: &str) -> Child {
        Command::new("sh")
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap()
    }

    #[test]
    fn poll_loop_is_bounded_when_exit_is_never_reported() {
        // A child that always reports "not exited" (e.g. a continuous EINTR
        // stream mapped to Ok(false)) must still stop at the deadline.
        let start = Instant::now();
        let outcome =
            poll_until_exit_or_deadline(start + Duration::from_millis(50), || Ok(false)).unwrap();
        assert!(!outcome, "must time out to Ok(false), not loop forever");
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn poll_loop_surfaces_check_errors() {
        let result = poll_until_exit_or_deadline(Instant::now() + Duration::from_secs(5), || {
            Err(Error::from(ErrorKind::Other))
        });
        assert!(result.is_err());
    }

    #[test]
    fn reports_natural_exit_status() {
        let mut child = spawn_group("exit 0");
        let status = await_exit_then_kill_group(&mut child, Duration::from_secs(5)).unwrap();
        assert!(status.is_some());
        assert!(status.unwrap().success());
    }

    #[test]
    fn reports_failure_exit_status() {
        let mut child = spawn_group("exit 3");
        let status = await_exit_then_kill_group(&mut child, Duration::from_secs(5)).unwrap();
        assert_eq!(status.and_then(|status| status.code()), Some(3));
    }

    #[test]
    fn preserves_parent_output_and_unblocks_reader_on_a_lingering_grandchild() {
        use std::io::Read;
        use std::sync::atomic::{AtomicU32, Ordering};

        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let flag = std::env::temp_dir().join(format!(
            "vt-proc-gc-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&flag);

        // Parent writes its output, then backgrounds a subshell that signals it
        // started (the flag file), inherits stdout, and outlives the parent.
        let script = format!(
            "( echo started > {flag} ; sleep 30 ) & printf PARENT ; exit 0",
            flag = flag.display()
        );
        let mut child = Command::new("sh")
            .args(["-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = child.id() as i32;
        let mut stdout = child.stdout.take().unwrap();
        let reader = std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = stdout.read_to_end(&mut buffer);
            buffer
        });

        // Confirm the descendant actually started before terminating.
        let waited = Instant::now();
        while !flag.exists() {
            assert!(
                waited.elapsed() < Duration::from_secs(3),
                "grandchild never signalled start"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let start = Instant::now();
        let status = await_exit_then_kill_group(&mut child, Duration::from_secs(10)).unwrap();
        let output = reader.join().unwrap();

        assert!(status.is_some_and(|status| status.success()));
        assert_eq!(
            output, b"PARENT",
            "parent output must survive the group kill"
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "reader must not block on the lingering grandchild"
        );
        // The killed descendant can remain visible briefly while init reaps
        // it, so wait for the process group to disappear instead of racing
        // that cleanup.
        let killed_by = Instant::now();
        loop {
            if unsafe { libc::kill(-pgid, 0) } == -1 {
                break;
            }
            assert!(
                killed_by.elapsed() < Duration::from_secs(3),
                "the whole group must be gone"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = std::fs::remove_file(&flag);
    }

    #[test]
    fn times_out_and_kills_the_group() {
        let mut child = spawn_group("sleep 30 & wait");
        let pgid = child.id() as i32;
        let start = Instant::now();
        let status = await_exit_then_kill_group(&mut child, Duration::from_millis(100)).unwrap();
        assert!(
            status.is_none(),
            "timed-out run must not report an exit status"
        );
        assert!(start.elapsed() < Duration::from_secs(2));
        // The whole group must be torn down within a moment.
        let killed_by = Instant::now();
        loop {
            if unsafe { libc::kill(-pgid, 0) } == -1 {
                break;
            }
            assert!(
                killed_by.elapsed() < Duration::from_secs(3),
                "descendant survived group termination"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
