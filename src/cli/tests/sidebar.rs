use super::*;

#[test]
fn dispatch_sidebar_attach_once_marks_and_renders() {
    let mock = MockTmuxRunner::new();
    let env = BTreeMap::from([("TMUX_PANE".to_string(), "%9".to_string())]);
    mock.stub(
        &[
            "set-option",
            "-p",
            "-t",
            "%9",
            crate::options::KEY_SIDEBAR_MARKER,
            "1",
        ],
        "",
    );
    let format = crate::options::snapshot::snapshot_format();
    let line = [
        "main", "@1", "%1", "/tmp/app", "zsh", "", "codex", "running", "", "", "", "", "", "", "",
        "",
    ]
    .join("\u{1f}");
    mock.stub(&["list-panes", "-a", "-F", &format], &format!("{line}\n"));

    let output = crate::cli::sidebar::run_sidebar_command_with_ensure(
        crate::cli::sidebar::SidebarCommand::Attach { once: true },
        &mock,
        &env,
        &crate::config::Config::default(),
        |_| Ok(()),
    )
    .unwrap();

    assert!(output.unwrap().contains("codex %1"));
}

#[test]
fn dispatch_sidebar_attach_once_restores_persisted_state() {
    let state_home = std::env::temp_dir().join(format!(
        "vde-tmux-sidebar-state-cli-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let env = BTreeMap::from([
        ("TMUX_PANE".to_string(), "%9".to_string()),
        (
            "XDG_STATE_HOME".to_string(),
            state_home.display().to_string(),
        ),
    ]);
    let state_path = crate::sidebar::store::state_path(&env);
    let mut state = crate::sidebar::state::SidebarState {
        selection: Some("repo::misc::app".to_string()),
        ..crate::sidebar::state::SidebarState::default()
    };
    state.collapsed.insert("repo::misc::app".to_string());
    crate::sidebar::store::save_state(&state_path, &state).unwrap();

    let mock = MockTmuxRunner::new();
    mock.stub(
        &[
            "set-option",
            "-p",
            "-t",
            "%9",
            crate::options::KEY_SIDEBAR_MARKER,
            "1",
        ],
        "",
    );
    let format = crate::options::snapshot::snapshot_format();
    let line = [
        "main", "@1", "%1", "/tmp/app", "zsh", "", "codex", "running", "", "", "", "", "", "", "",
        "",
    ]
    .join("\u{1f}");
    mock.stub(&["list-panes", "-a", "-F", &format], &format!("{line}\n"));

    let output = crate::cli::sidebar::run_sidebar_command_with_ensure(
        crate::cli::sidebar::SidebarCommand::Attach { once: true },
        &mock,
        &env,
        &crate::config::Config::default(),
        |_| Ok(()),
    )
    .unwrap();
    let output = output.unwrap();

    assert!(output.contains("> > app"));
    assert!(!output.contains("codex %1"));
    std::fs::remove_dir_all(state_home).unwrap();
}

#[test]
fn dispatch_sidebar_open_uses_layout_operations() {
    let mock = MockTmuxRunner::new();
    let exe = std::env::current_exe().unwrap();
    let command = format!(
        "{} sidebar attach",
        shell_quote_for_test(&exe.display().to_string())
    );
    mock.stub(
        &[
            "list-panes",
            "-t",
            "@1",
            "-F",
            crate::sidebar::layout::SIDEBAR_PANE_FORMAT,
        ],
        "%1\t\t80\n",
    );
    mock.stub(
        &[
            "display-message",
            "-p",
            "-t",
            "@1",
            "-F",
            "#{window_layout}",
        ],
        "layout-before\n",
    );
    mock.stub(&["list-panes", "-t", "@1", "-F", "#{pane_id}"], "%1\n");
    mock.stub(
        &[
            "set-option",
            "-w",
            "-t",
            "@1",
            crate::options::KEY_LAYOUT_BASELINE,
            "layout-before",
        ],
        "",
    );
    mock.stub(
        &[
            "set-option",
            "-w",
            "-t",
            "@1",
            crate::options::KEY_LAYOUT_PANES,
            "%1",
        ],
        "",
    );
    mock.stub(
        &["split-window", "-t", "@1", "-hbf", "-l", "40", &command],
        "",
    );

    crate::cli::sidebar::run_sidebar_command_with_ensure(
        crate::cli::sidebar::SidebarCommand::Open {
            window: Some("@1".to_string()),
            width: Some(40),
            delay_ms: Some(0),
        },
        &mock,
        &env(),
        &crate::config::Config::default(),
        |_| Ok(()),
    )
    .unwrap();

    assert_eq!(mock.calls().len(), 6);
}

#[test]
fn dispatch_sidebar_toggle_all_uses_all_windows() {
    let mock = MockTmuxRunner::new();
    let exe = std::env::current_exe().unwrap();
    let command = format!(
        "{} sidebar attach",
        shell_quote_for_test(&exe.display().to_string())
    );
    mock.stub(&["list-windows", "-a", "-F", "#{window_id}"], "@1\n");
    mock.stub(
        &[
            "list-panes",
            "-t",
            "@1",
            "-F",
            crate::sidebar::layout::SIDEBAR_PANE_FORMAT,
        ],
        "%1\t\t80\n",
    );
    mock.stub(
        &[
            "display-message",
            "-p",
            "-t",
            "@1",
            "-F",
            "#{window_layout}",
        ],
        "layout-before\n",
    );
    mock.stub(&["list-panes", "-t", "@1", "-F", "#{pane_id}"], "%1\n");
    mock.stub(
        &[
            "set-option",
            "-w",
            "-t",
            "@1",
            crate::options::KEY_LAYOUT_BASELINE,
            "layout-before",
        ],
        "",
    );
    mock.stub(
        &[
            "set-option",
            "-w",
            "-t",
            "@1",
            crate::options::KEY_LAYOUT_PANES,
            "%1",
        ],
        "",
    );
    mock.stub(
        &["split-window", "-t", "@1", "-hbf", "-l", "40", &command],
        "",
    );

    crate::cli::sidebar::run_sidebar_command_with_ensure(
        crate::cli::sidebar::SidebarCommand::Toggle {
            all: true,
            window: None,
            width: Some(40),
        },
        &mock,
        &env(),
        &crate::config::Config::default(),
        |_| Ok(()),
    )
    .unwrap();

    assert_eq!(mock.calls().len(), 7);
}

#[test]
fn dispatch_sidebar_jump_switches_to_pane() {
    let mock = MockTmuxRunner::new();
    let format = crate::options::snapshot::snapshot_format();
    let line = [
        "main", "@1", "%1", "/tmp/app", "zsh", "", "codex", "running", "", "", "", "", "", "", "",
        "",
    ]
    .join("\u{1f}");
    mock.stub(&["list-panes", "-a", "-F", &format], &format!("{line}\n"));
    mock.stub(&["switch-client", "-t", "main"], "");
    mock.stub(&["select-window", "-t", "@1"], "");
    mock.stub(&["select-pane", "-t", "%1"], "");

    run_with(["vt", "sidebar", "jump", "%1"], &mock, &env()).unwrap();

    assert_eq!(mock.calls().len(), 4);
}

#[test]
fn dispatch_sidebar_input_moves_selection_and_saves_state() {
    let state_home = std::env::temp_dir().join(format!(
        "vde-tmux-sidebar-input-cli-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let env = BTreeMap::from([(
        "XDG_STATE_HOME".to_string(),
        state_home.display().to_string(),
    )]);
    let mock = MockTmuxRunner::new();
    let format = crate::options::snapshot::snapshot_format();
    let line = [
        "main", "@1", "%1", "/tmp/app", "zsh", "", "codex", "running", "", "", "", "", "", "", "",
        "",
    ]
    .join("\u{1f}");
    mock.stub(&["list-panes", "-a", "-F", &format], &format!("{line}\n"));

    run_with(["vt", "sidebar", "input", "j"], &mock, &env).unwrap();

    let state =
        crate::sidebar::store::load_state(&crate::sidebar::store::state_path(&env)).unwrap();
    assert_eq!(state.selection.as_deref(), Some("repo::misc::app"));
    std::fs::remove_dir_all(state_home).unwrap();
}

#[test]
fn sidebar_layout_applied_ensures_daemon_started() {
    use std::cell::Cell;

    let mock = MockTmuxRunner::new();
    let exe = std::env::current_exe().unwrap();
    let command = format!(
        "{} sidebar attach",
        shell_quote_for_test(&exe.display().to_string())
    );
    mock.stub(&["display-message", "-p", "#{window_id}"], "@1\n");
    mock.stub(
        &[
            "list-panes",
            "-t",
            "@1",
            "-F",
            crate::sidebar::layout::SIDEBAR_PANE_FORMAT,
        ],
        "%1\t\t80\n",
    );
    mock.stub(
        &[
            "display-message",
            "-p",
            "-t",
            "@1",
            "-F",
            "#{window_layout}",
        ],
        "layout-before\n",
    );
    mock.stub(&["list-panes", "-t", "@1", "-F", "#{pane_id}"], "%1\n");
    mock.stub(
        &[
            "set-option",
            "-w",
            "-t",
            "@1",
            crate::options::KEY_LAYOUT_BASELINE,
            "layout-before",
        ],
        "",
    );
    mock.stub(
        &[
            "set-option",
            "-w",
            "-t",
            "@1",
            crate::options::KEY_LAYOUT_PANES,
            "%1",
        ],
        "",
    );
    mock.stub(
        &["split-window", "-t", "@1", "-hbf", "-l", "40", &command],
        "",
    );
    let called = Cell::new(false);

    crate::cli::sidebar::run_sidebar_command_with_ensure(
        crate::cli::sidebar::SidebarCommand::LayoutApplied {
            window: Some("@1".to_string()),
            width: Some(40),
        },
        &mock,
        &env(),
        &crate::config::Config::default(),
        |_| {
            called.set(true);
            Ok(())
        },
    )
    .unwrap();

    assert!(called.get());
}

fn shell_quote_for_test(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}
