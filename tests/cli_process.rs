use std::process::{Command, Output};

fn run_vt(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_vt"))
        .args(args)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output()
        .unwrap_or_else(|error| panic!("failed to run vt {args:?}: {error}"))
}

#[test]
fn help_and_version_write_to_stdout_and_exit_zero() {
    for (args, expected) in [
        (&["--help"][..], "Usage: vt <COMMAND>"),
        (&["sidebar", "open", "--help"][..], "Usage: vt sidebar open"),
        (&["--version"][..], env!("CARGO_PKG_VERSION")),
    ] {
        let output = run_vt(args);
        assert_eq!(
            output.status.code(),
            Some(0),
            "vt {args:?} stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(expected),
            "vt {args:?} stdout: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            output.stderr.is_empty(),
            "vt {args:?} unexpectedly wrote stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn invalid_arguments_write_to_stderr_and_exit_two() {
    let output = run_vt(&["definitely-not-a-command"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unrecognized subcommand"));

    let output = run_vt(&["agent", "list", "--definitely-invalid", "--json"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let error: serde_json::Value = serde_json::from_slice(&output.stderr)
        .unwrap_or_else(|parse_error| panic!("invalid JSON API error: {parse_error}"));
    assert_eq!(error["error"]["code"], "invalid_arguments");
}

#[test]
fn runtime_errors_remain_on_stderr_with_exit_one() {
    let output = run_vt(&["daemon", "status"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("TMUX is required"));
}
