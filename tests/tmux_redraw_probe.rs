#![cfg(unix)]

use std::fs::File;
use std::io::{self, Read};
use std::os::fd::FromRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use vde_tmux::config::Config;
use vde_tmux::daemon::protocol::v2::PanePresentation;
use vde_tmux::daemon::session_badge::BadgeState;
use vde_tmux::pane_state::{
    AgentKind, LifecycleState, PANE_STATE_SCHEMA_VERSION, PaneInstance, PaneState,
    ResolvedPaneState, StateId, TaskState,
};

const SESSION: &str = "vde-redraw-probe";
static PROBE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

struct ProbeServer {
    socket_name: String,
    client: Option<Child>,
    drain: Option<thread::JoinHandle<()>>,
}

impl ProbeServer {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock must be after the Unix epoch")
            .as_nanos();
        Self {
            socket_name: format!(
                "vde-redraw-probe-{}-{nonce}-{}",
                std::process::id(),
                PROBE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ),
            client: None,
            drain: None,
        }
    }

    fn tmux(&self, args: &[&str]) -> Output {
        Command::new("tmux")
            .args(["-L", &self.socket_name, "-f", "/dev/null"])
            .args(args)
            .output()
            .unwrap_or_else(|error| panic!("failed to run tmux {args:?}: {error}"))
    }

    fn tmux_ok(&self, args: &[&str]) -> String {
        let output = self.tmux(args);
        assert!(
            output.status.success(),
            "tmux {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("tmux output must be UTF-8")
            .trim()
            .to_string()
    }

    fn attach_client(&mut self) {
        let mut master_fd = -1;
        let mut slave_fd = -1;
        let mut size = libc::winsize {
            ws_row: 133,
            ws_col: 640,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: openpty initializes both file descriptors on success and only
        // reads the supplied winsize value.
        let result = unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        };
        assert_eq!(result, 0, "openpty failed: {}", io::Error::last_os_error());
        // SAFETY: slave_fd is an open PTY descriptor and size points to a
        // fully initialized winsize value.
        let resize = unsafe { libc::ioctl(slave_fd, libc::TIOCSWINSZ as _, &size) };
        assert_eq!(
            resize,
            0,
            "TIOCSWINSZ failed: {}",
            io::Error::last_os_error()
        );

        // SAFETY: openpty returned unique owned descriptors above.
        let mut master = unsafe { File::from_raw_fd(master_fd) };
        // SAFETY: openpty returned unique owned descriptors above.
        let slave = unsafe { File::from_raw_fd(slave_fd) };
        let stdin = slave.try_clone().expect("clone PTY slave for stdin");
        let stdout = slave.try_clone().expect("clone PTY slave for stdout");
        let stderr = slave.try_clone().expect("clone PTY slave for stderr");

        let mut command = Command::new("tmux");
        command
            .args([
                "-L",
                &self.socket_name,
                "-f",
                "/dev/null",
                "attach-session",
                "-t",
                SESSION,
            ])
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        // SAFETY: this runs after fork and before exec. stdin is the PTY slave,
        // and no allocation or shared-state access is performed in the closure.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        self.client = Some(command.spawn().expect("spawn attached tmux client"));
        drop(slave);
        self.drain = Some(thread::spawn(move || {
            let mut buffer = [0u8; 64 * 1024];
            while master.read(&mut buffer).is_ok_and(|read| read > 0) {}
        }));
    }

    fn client_counters(&self) -> (u64, u64) {
        let output = self.tmux_ok(&[
            "list-clients",
            "-F",
            "#{client_written}|#{client_discarded}",
        ]);
        let rows = output.lines().collect::<Vec<_>>();
        assert_eq!(
            rows.len(),
            1,
            "probe must have exactly one client: {rows:?}"
        );
        let (written, discarded) = rows[0]
            .split_once('|')
            .expect("client counters must contain a separator");
        (
            written.parse().expect("client_written must be numeric"),
            discarded.parse().expect("client_discarded must be numeric"),
        )
    }

    fn wait_for_client(&self) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let output = self.tmux(&["list-clients", "-F", "#{client_written}"]);
            if output.status.success() && !output.stdout.is_empty() {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
        panic!("attached tmux client did not appear within three seconds");
    }

    fn wait_for_pane_command(&self, pane: &str, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let command = self.tmux_ok(&[
                "display-message",
                "-p",
                "-t",
                pane,
                "#{pane_current_command}",
            ]);
            if command == expected {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
        panic!("pane {pane} did not reach command {expected:?} within three seconds");
    }

    fn wait_until_written_increases(&self, baseline: u64) -> u64 {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            let (written, discarded) = self.client_counters();
            assert_eq!(discarded, 0, "probe client discarded output");
            if written > baseline {
                return written;
            }
            thread::sleep(Duration::from_millis(25));
        }
        panic!("client_written did not increase within two seconds");
    }
}

impl Drop for ProbeServer {
    fn drop(&mut self) {
        if let Some(client) = self.client.as_mut() {
            let _ = client.kill();
            let _ = client.wait();
        }
        let _ = self.tmux(&["kill-server"]);
        if let Some(drain) = self.drain.take() {
            let _ = drain.join();
        }
    }
}

fn assert_tmux_3_7_or_newer() {
    let output = Command::new("tmux")
        .arg("-V")
        .output()
        .expect("tmux must be installed");
    assert!(output.status.success(), "tmux -V must succeed");
    let version = String::from_utf8(output.stdout).expect("tmux version must be UTF-8");
    let number = version
        .trim()
        .strip_prefix("tmux ")
        .expect("tmux version must start with 'tmux '");
    let mut parts = number.split(['.', 'a', 'b', 'c']);
    let major = parts
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .expect("tmux major version must be numeric");
    let minor = parts
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .expect("tmux minor version must be numeric");
    assert!(
        (major, minor) >= (3, 7),
        "requires tmux 3.7+, found {version}"
    );
}

fn running_pane_presentation(epoch: i64) -> PanePresentation {
    let pane_instance = PaneInstance {
        pane_id: "%7".to_string(),
        pane_pid: 700,
    };
    let canonical = PaneState {
        schema_version: PANE_STATE_SCHEMA_VERSION,
        state_id: StateId::parse("00000000000000000000000000000007").unwrap(),
        revision: 1,
        pane_instance: pane_instance.clone(),
        agent: AgentKind::parse("codex").unwrap(),
        agent_session_id: None,
        agent_epoch: 1,
        agent_present: true,
        scan_verified: true,
        synthetic_completion_armed: false,
        lifecycle: LifecycleState::Running,
        run_seq: 1,
        completed_seq: 0,
        acknowledged_seq: 0,
        started_at: Some(epoch),
        completed_at: None,
        prompt: None,
        tasks: TaskState::default(),
        subagents: Vec::new(),
        worktree_activity: None,
    };
    PanePresentation {
        pane_instance: pane_instance.clone(),
        session_links: Vec::new(),
        window_id: "@1".to_string(),
        window_name: "probe".to_string(),
        current_path: "/tmp".to_string(),
        current_command: "codex".to_string(),
        pane_width: 80,
        active: true,
        stored: None,
        resolved: Some(ResolvedPaneState {
            canonical,
            window_id: "@1".to_string(),
            pane_id: pane_instance.pane_id.clone(),
            current_path: "/tmp".to_string(),
            badge: BadgeState::Working,
        }),
        diagnostic: None,
    }
}

#[test]
#[ignore = "requires tmux 3.7+"]
fn tmux_expands_dynamic_pane_elapsed_with_the_former_boundaries() {
    assert_tmux_3_7_or_newer();
    let probe = ProbeServer::new();
    probe.tmux_ok(&["new-session", "-d", "-s", SESSION]);
    let pane = probe.tmux_ok(&["display-message", "-p", "-t", SESSION, "#{pane_id}"]);
    let epoch = 1_000;
    let rendered = vde_tmux::statusline::render_structured_pane_status(
        &Config::default(),
        &running_pane_presentation(epoch),
    );
    probe.tmux_ok(&[
        "set-option",
        "-p",
        "-t",
        &pane,
        "@vde_status_pane",
        &rendered,
    ]);

    for (delta, expected) in [
        (0, "Running 0s"),
        (30, "Running 30s"),
        (60, "Running 1m00s"),
        (90, "Running 1m30s"),
        (599, "Running 9m59s"),
        (600, "Running 10m"),
        (5_400, "Running 1h30m"),
        (172_800, "Running 2d"),
    ] {
        probe.tmux_ok(&[
            "set-option",
            "-g",
            "@vde_status_now_format",
            &(epoch + delta).to_string(),
        ]);
        let expanded = probe.tmux_ok(&[
            "display-message",
            "-p",
            "-t",
            &pane,
            "#{E:@vde_status_pane}",
        ]);
        assert!(expanded.contains("%7"), "delta={delta}: {expanded}");
        assert!(expanded.contains(expected), "delta={delta}: {expanded}");
    }
}

#[test]
#[ignore = "requires tmux 3.7+"]
fn tmux_operations_have_expected_client_redraw_effects() {
    assert_tmux_3_7_or_newer();
    let mut probe = ProbeServer::new();
    probe.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        SESSION,
        "-x",
        "640",
        "-y",
        "133",
        "yes X | head -c 80000; exec sleep 300",
    ]);
    probe.tmux_ok(&["set-option", "-g", "status", "off"]);
    let visible = probe.tmux_ok(&["display-message", "-p", "-t", SESSION, "#{pane_id}"]);
    probe.wait_for_pane_command(&visible, "sleep");
    let hidden = probe.tmux_ok(&[
        "new-window",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-t",
        SESSION,
        "-n",
        "hidden",
    ]);

    probe.attach_client();
    probe.wait_for_client();
    // tmux waits for terminal capability replies after attach. This PTY is a
    // byte sink rather than a terminal emulator, so let that one-time probe
    // timeout complete before taking the idle baseline.
    thread::sleep(Duration::from_secs(6));

    let (baseline_written, baseline_discarded) = probe.client_counters();
    assert_eq!(baseline_discarded, 0, "probe client discarded output");
    thread::sleep(Duration::from_secs(5));
    let (idle_written, idle_discarded) = probe.client_counters();
    assert_eq!(idle_written, baseline_written, "idle client output changed");
    assert_eq!(idle_discarded, 0, "probe client discarded output");

    probe.tmux_ok(&["capture-pane", "-p", "-t", &visible]);
    probe.tmux_ok(&["list-panes", "-a", "-F", "#{pane_id}|#{pane_pid}"]);
    thread::sleep(Duration::from_millis(500));
    let (read_only_written, read_only_discarded) = probe.client_counters();
    assert_eq!(
        read_only_written, idle_written,
        "read-only tmux commands wrote to the attached client"
    );
    assert_eq!(read_only_discarded, 0, "probe client discarded output");

    probe.tmux_ok(&[
        "set-option",
        "-p",
        "-t",
        &hidden,
        "@vde_redraw_probe",
        "tick",
    ]);
    let increased = probe.wait_until_written_increases(read_only_written);
    assert!(increased > read_only_written);
}
