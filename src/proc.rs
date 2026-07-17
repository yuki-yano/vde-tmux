//! Bounded subprocess termination that also reaps descendants.
//!
//! A child spawned in its own process group can leave a descendant that
//! inherited its stdout/stderr; the reader threads then block until that
//! descendant exits. Killing only the group leader does not help, and killing
//! the group *after* reaping the leader risks signalling a reused pgid. These
//! helpers detect the leader's exit without reaping it (so the pgid stays
//! valid), kill the whole group, and only then reap.

use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

/// Poll whether `pid` has terminated, without reaping it (`WNOWAIT`), so the
/// process — and therefore its process group id — stays valid for a following
/// group kill. Returns true once the child has exited.
fn child_exited_without_reaping(pid: i32) -> bool {
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
    // With WNOHANG, si_signo stays 0 while the child is still running and is set
    // to SIGCHLD once it has terminated.
    rc == 0 && info.si_signo != 0
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
/// leader. Returns the exit status if the child exited before the timeout, or
/// `None` if it had to be killed.
///
/// The group kill happens before the leader is reaped, so any descendant that
/// inherited the child's pipes is terminated and the pgid cannot be reused.
pub fn await_exit_then_kill_group(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let pid = child.id() as i32;
    let deadline = Instant::now() + timeout;
    let mut exited = false;
    loop {
        if child_exited_without_reaping(pid) {
            exited = true;
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    // The leader is still present here (a zombie if it exited, alive on
    // timeout), so its pgid is valid.
    kill_process_group(pid);
    let status = child.wait().ok();
    if exited { status } else { None }
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
    fn reports_natural_exit_status() {
        let mut child = spawn_group("exit 0");
        let status = await_exit_then_kill_group(&mut child, Duration::from_secs(5));
        assert!(status.is_some());
        assert!(status.unwrap().success());
    }

    #[test]
    fn reports_failure_exit_status() {
        let mut child = spawn_group("exit 3");
        let status = await_exit_then_kill_group(&mut child, Duration::from_secs(5));
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
        let status = await_exit_then_kill_group(&mut child, Duration::from_secs(10));
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
        assert_eq!(
            unsafe { libc::kill(-pgid, 0) },
            -1,
            "the whole group must be gone"
        );
        let _ = std::fs::remove_file(&flag);
    }

    #[test]
    fn times_out_and_kills_the_group() {
        let mut child = spawn_group("sleep 30 & wait");
        let pgid = child.id() as i32;
        let start = Instant::now();
        let status = await_exit_then_kill_group(&mut child, Duration::from_millis(100));
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
