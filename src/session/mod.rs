use std::collections::BTreeMap;

use anyhow::{Result, anyhow, bail};

use crate::category::{adjacent_category, resolve_category_for_session, sessions_in_category};
use crate::config::Config;
use crate::options::{
    KEY_CATEGORY, KEY_PROJECT_PATH, set_global_option, set_session_option, show_global_option,
};
use crate::tmux::TmuxRunner;

const FIELD_SEP: char = '\u{1f}';

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionInfo {
    pub name: String,
    pub attached: bool,
    pub created_at: i64,
    pub project_path: String,
    pub id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Next,
    Previous,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientSessionContext {
    pub client_name: String,
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClientSessionRow {
    context: ClientSessionContext,
    pane_id: String,
}

pub fn session_list_format() -> String {
    [
        "#{session_name}",
        "#{session_attached}",
        "#{session_created}",
        "",
        "#{@vde_project_path}",
        "",
        "#{session_id}",
    ]
    .join(&FIELD_SEP.to_string())
}

pub fn parse_sessions(output: &str) -> Vec<SessionInfo> {
    output
        .lines()
        .filter_map(|line| {
            let fields = line.split(FIELD_SEP).collect::<Vec<_>>();
            if fields.len() != 7 {
                return None;
            }
            Some(SessionInfo {
                name: fields[0].to_string(),
                attached: fields[1] == "1",
                created_at: fields[2].parse().unwrap_or_default(),
                project_path: fields[4].to_string(),
                id: fields[6].to_string(),
            })
        })
        .collect()
}

pub fn list_sessions(runner: &dyn TmuxRunner) -> Result<Vec<SessionInfo>> {
    let format = session_list_format();
    let output = runner.run(&["list-sessions", "-F", &format])?;
    Ok(parse_sessions(&output))
}

pub fn current_client_name(runner: &dyn TmuxRunner) -> Result<String> {
    if let Ok(client) = runner.run(&["display-message", "-p", "#{client_name}\t#{client_tty}"]) {
        let client = parse_client_target_line(client.trim());
        if !client.is_empty() {
            return Ok(client.to_string());
        }
    }
    Ok(runner
        .run(&["list-clients", "-F", "#{client_name}\t#{client_tty}"])?
        .lines()
        .map(parse_client_target_line)
        .find(|client| !client.is_empty())
        .unwrap_or_default()
        .to_string())
}

fn parse_client_target_line(line: &str) -> &str {
    let (name, tty) = line.split_once('\t').unwrap_or((line, ""));
    let name = name.trim();
    if !name.is_empty() {
        return name;
    }
    tty.trim()
}

pub fn current_session_name(runner: &dyn TmuxRunner) -> Result<String> {
    Ok(runner
        .run(&["display-message", "-p", "#{session_name}"])?
        .trim()
        .to_string())
}

pub fn current_session_id(runner: &dyn TmuxRunner) -> Result<String> {
    let session_id = runner
        .run(&["display-message", "-p", "#{session_id}"])?
        .trim()
        .to_string();
    if !valid_session_id(&session_id) {
        return Err(anyhow!("tmux did not return a valid current session ID"));
    }
    Ok(session_id)
}

pub fn client_session_context_format() -> String {
    [
        "#{client_name}",
        "#{client_tty}",
        "#{session_id}",
        "#{pane_id}",
        "#{client_control_mode}",
    ]
    .join(&FIELD_SEP.to_string())
}

pub fn client_pid_name_format() -> String {
    [
        "#{client_pid}",
        "#{client_name}",
        "#{client_tty}",
        "#{client_control_mode}",
    ]
    .join(&FIELD_SEP.to_string())
}

pub fn regular_client_name_for_pid(runner: &dyn TmuxRunner, requested_pid: u32) -> Result<String> {
    if requested_pid == 0 {
        bail!("explicit tmux client PID must be positive");
    }
    let format = client_pid_name_format();
    let output = runner.run(&["list-clients", "-F", &format])?;
    let matches = output
        .lines()
        .filter_map(|line| {
            let fields = line.split(FIELD_SEP).collect::<Vec<_>>();
            if fields.len() != 4
                || fields[0].parse::<u32>().ok() != Some(requested_pid)
                || fields[3] != "0"
            {
                return None;
            }
            let client_name = if fields[1].trim().is_empty() {
                fields[2].trim()
            } else {
                fields[1].trim()
            };
            (!client_name.is_empty()).then(|| client_name.to_string())
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [client_name] => Ok(client_name.clone()),
        [] => bail!("regular tmux client PID {requested_pid} is not attached"),
        _ => bail!("tmux returned multiple regular clients for PID {requested_pid}"),
    }
}

pub fn client_session_context_for_pane(
    runner: &dyn TmuxRunner,
    pane_id: &str,
    requested_client_name: Option<&str>,
) -> Result<ClientSessionContext> {
    let valid_pane = pane_id.strip_prefix('%').is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    });
    if !valid_pane {
        return Err(anyhow!("invalid invoking tmux pane ID: {pane_id}"));
    }
    let rows = regular_client_session_rows(runner)?;
    let mut matches = rows
        .iter()
        .filter(|row| {
            row.pane_id == pane_id
                && requested_client_name
                    .is_none_or(|requested| requested == row.context.client_name)
        })
        .map(|row| row.context.clone())
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| {
        left.client_name
            .cmp(&right.client_name)
            .then_with(|| left.session_id.cmp(&right.session_id))
    });
    matches.dedup();
    match matches.as_slice() {
        [context] => Ok(context.clone()),
        [] => {
            if requested_client_name.is_none() {
                // A stale TMUX_PANE inherited by run-shell is safe to ignore only when the
                // server has exactly one regular client. Multiple clients must fail closed
                // and capture --client-name in the binding instead.
                let mut all_clients = rows.into_iter().map(|row| row.context).collect::<Vec<_>>();
                all_clients.sort_by(|left, right| {
                    left.client_name
                        .cmp(&right.client_name)
                        .then_with(|| left.session_id.cmp(&right.session_id))
                });
                all_clients.dedup();
                if let [context] = all_clients.as_slice() {
                    return Ok(context.clone());
                }
            }
            match requested_client_name {
                Some(client_name) => Err(anyhow!(
                    "regular tmux client {client_name} is not displaying invoking pane {pane_id}"
                )),
                None => Err(anyhow!(
                    "invoking pane {pane_id} does not identify one regular tmux client; capture --client-name in the tmux binding"
                )),
            }
        }
        _ => Err(anyhow!(
            "multiple tmux clients are displaying invoking pane {pane_id}; capture --client-name in the tmux binding"
        )),
    }
}

pub fn client_session_context_for_client(
    runner: &dyn TmuxRunner,
    requested_client_name: &str,
) -> Result<ClientSessionContext> {
    if requested_client_name.trim().is_empty() {
        return Err(anyhow!("explicit tmux client name must not be empty"));
    }
    let mut matches = regular_client_session_rows(runner)?
        .into_iter()
        .filter(|row| row.context.client_name == requested_client_name)
        .map(|row| row.context)
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| {
        left.client_name
            .cmp(&right.client_name)
            .then_with(|| left.session_id.cmp(&right.session_id))
    });
    matches.dedup();
    match matches.as_slice() {
        [context] => Ok(context.clone()),
        [] => Err(anyhow!(
            "regular tmux client {requested_client_name} is not attached"
        )),
        _ => Err(anyhow!(
            "tmux returned multiple contexts for client {requested_client_name}"
        )),
    }
}

fn regular_client_session_rows(runner: &dyn TmuxRunner) -> Result<Vec<ClientSessionRow>> {
    let format = client_session_context_format();
    let output = runner.run(&["list-clients", "-F", &format])?;
    Ok(output
        .lines()
        .filter_map(|line| {
            let fields = line.split(FIELD_SEP).collect::<Vec<_>>();
            if fields.len() != 5 || fields[4] != "0" {
                return None;
            }
            let client_name = if fields[0].trim().is_empty() {
                fields[1].trim()
            } else {
                fields[0].trim()
            };
            let session_id = fields[2].trim();
            let pane_id = fields[3].trim();
            if client_name.is_empty() || !valid_session_id(session_id) || !valid_pane_id(pane_id) {
                return None;
            }
            Some(ClientSessionRow {
                context: ClientSessionContext {
                    client_name: client_name.to_string(),
                    session_id: session_id.to_string(),
                },
                pane_id: pane_id.to_string(),
            })
        })
        .collect())
}

fn valid_pane_id(pane_id: &str) -> bool {
    pane_id.strip_prefix('%').is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn valid_session_id(session_id: &str) -> bool {
    session_id.strip_prefix('$').is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    })
}

pub fn switch_client(runner: &dyn TmuxRunner, session: &str) -> Result<()> {
    let target = exact_session_target(session);
    runner.run(&["switch-client", "-t", &target])?;
    Ok(())
}

pub fn exact_session_target(session: &str) -> String {
    format!("={session}:")
}

pub fn switch_client_for_client(
    runner: &dyn TmuxRunner,
    client_name: &str,
    session: &str,
) -> Result<()> {
    if client_name.trim().is_empty() {
        return switch_client(runner, session);
    }
    let target = exact_session_target(session);
    runner.run(&["switch-client", "-c", client_name, "-t", &target])?;
    Ok(())
}

pub fn client_memory_key(client_name: &str, category: &str) -> String {
    let hex = client_name
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let safe_category = category
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    format!("@vde_client_{hex}_{safe_category}")
}

pub fn remember_session_for_client(
    runner: &dyn TmuxRunner,
    client_name: &str,
    category: &str,
    session_name: &str,
) -> Result<()> {
    set_global_option(
        runner,
        &client_memory_key(client_name, category),
        session_name,
    )
}

pub fn remembered_session_for_client(
    runner: &dyn TmuxRunner,
    client_name: &str,
    category: &str,
) -> Result<Option<String>> {
    show_global_option(runner, &client_memory_key(client_name, category))
}

pub fn find_session<'a>(sessions: &'a [SessionInfo], name: &str) -> Option<&'a SessionInfo> {
    sessions.iter().find(|session| session.name == name)
}

pub fn remember_client_session_for_session(
    runner: &dyn TmuxRunner,
    config: &Config,
    client_name: &str,
    session_name: &str,
) -> Result<()> {
    let sessions = list_sessions(runner)?;
    let session = find_session(&sessions, session_name)
        .ok_or_else(|| anyhow!("session not found: {session_name}"))?;
    let category = resolve_category_for_session(config, session);
    remember_session_for_client(runner, client_name, &category, session_name)
}

pub fn remember_current_client_session(runner: &dyn TmuxRunner, config: &Config) -> Result<()> {
    let client = current_client_name(runner)?;
    let current = current_session_name(runner)?;
    remember_client_session_for_session(runner, config, &client, &current)
}

pub fn create_session(
    runner: &dyn TmuxRunner,
    config: &Config,
    env: &BTreeMap<String, String>,
    cwd: Option<&str>,
) -> Result<String> {
    let client = current_client_name(runner)?;
    create_session_for_client(runner, config, env, cwd, &client)
}

pub fn create_session_for_client(
    runner: &dyn TmuxRunner,
    config: &Config,
    env: &BTreeMap<String, String>,
    cwd: Option<&str>,
    client: &str,
) -> Result<String> {
    if client.trim().is_empty() {
        bail!("no tmux client available for session creation");
    }
    let cwd = resolve_session_cwd(runner, env, cwd)?;
    let created_format = format!("#{{session_name}}{FIELD_SEP}#{{window_id}}");
    let created = runner.run(&["new-session", "-d", "-P", "-F", &created_format, "-c", &cwd])?;
    let (session_name, window_id) = parse_created_session(&created)?;
    set_session_option(runner, &session_name, KEY_PROJECT_PATH, &cwd)?;
    let session = SessionInfo {
        name: session_name.clone(),
        project_path: cwd,
        ..SessionInfo::default()
    };
    let category = resolve_category_for_session(config, &session);
    set_session_option(runner, &session_name, KEY_CATEGORY, &category)?;
    switch_client_for_client(runner, client, &session_name)?;
    if !window_id.is_empty() {
        crate::sidebar::layout::open_if_auto_all_enabled(
            runner,
            &window_id,
            &std::env::current_exe()?,
            config.sidebar.width,
            config.sidebar.min_width,
        )?;
    }
    if !category.is_empty() {
        remember_session_for_client(runner, client, &category, &session_name)?;
    }
    Ok(session_name)
}

fn parse_created_session(output: &str) -> Result<(String, String)> {
    let line = output.trim();
    let Some((session_name, window_id)) = line.split_once(FIELD_SEP) else {
        bail!("tmux did not return session/window ids for new session");
    };
    let session_name = session_name.trim().to_string();
    if session_name.is_empty() {
        bail!("tmux did not return a new session name");
    }
    Ok((session_name, window_id.trim().to_string()))
}

fn resolve_session_cwd(
    runner: &dyn TmuxRunner,
    env: &BTreeMap<String, String>,
    cwd: Option<&str>,
) -> Result<String> {
    let cwd = match cwd.map(str::trim).filter(|cwd| !cwd.is_empty()) {
        Some(cwd) => cwd.to_string(),
        None => runner
            .run(&["display-message", "-p", "#{pane_current_path}"])?
            .trim()
            .to_string(),
    };
    let cwd = expand_tilde_path(&cwd, env.get("HOME").map(String::as_str));
    if cwd.trim().is_empty() {
        bail!("session cwd is empty");
    }
    Ok(cwd)
}

fn expand_tilde_path(path: &str, home: Option<&str>) -> String {
    let Some(home) = home.filter(|home| !home.is_empty()) else {
        return path.to_string();
    };
    if path == "~" {
        return home.to_string();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if rest.is_empty() {
            home.to_string()
        } else {
            format!("{home}/{rest}")
        }
    } else {
        path.to_string()
    }
}

pub fn sync_session_category_mirrors(runner: &dyn TmuxRunner, config: &Config) -> Result<()> {
    for session in list_sessions(runner)? {
        let category = resolve_category_for_session(config, &session);
        set_session_option(runner, &session.name, KEY_CATEGORY, &category)?;
    }
    Ok(())
}

pub fn use_category(runner: &dyn TmuxRunner, config: &Config, category: &str) -> Result<()> {
    let client = current_client_name(runner)?;
    use_category_for_client(runner, config, category, &client)
}

pub fn use_category_for_client(
    runner: &dyn TmuxRunner,
    config: &Config,
    category: &str,
    client: &str,
) -> Result<()> {
    let sessions = list_sessions(runner)?;
    use_category_for_client_from_sessions(runner, config, &sessions, category, client)
}

pub(crate) fn use_category_for_client_from_sessions(
    runner: &dyn TmuxRunner,
    config: &Config,
    sessions: &[SessionInfo],
    category: &str,
    client: &str,
) -> Result<()> {
    if let Some(remembered) = remembered_session_for_client(runner, client, category)?
        && let Some(remembered_session) = find_session(sessions, &remembered)
        && resolve_category_for_session(config, remembered_session) == category
    {
        return switch_client_for_client(runner, client, &remembered);
    }

    let Some(session) = sessions_in_category(config, sessions, category)
        .first()
        .copied()
    else {
        bail!("no session in category: {category}");
    };
    switch_client_for_client(runner, client, &session.name)
}

pub fn use_adjacent_category(
    runner: &dyn TmuxRunner,
    config: &Config,
    direction: Direction,
) -> Result<()> {
    let client = current_client_name(runner)?;
    let current = current_session_name(runner)?;
    let sessions = list_sessions(runner)?;
    let session = find_session(&sessions, &current)
        .ok_or_else(|| anyhow!("current session not found: {current}"))?;
    let current_category = resolve_category_for_session(config, session);
    let next_category = adjacent_category(config, &sessions, &current_category, direction)
        .ok_or_else(|| anyhow!("no categories available"))?;
    use_category_for_client_from_sessions(runner, config, &sessions, &next_category, &client)
}

pub fn cycle_session(runner: &dyn TmuxRunner, config: &Config, direction: Direction) -> Result<()> {
    let client = current_client_name(runner)?;
    let current = current_session_name(runner)?;
    let sessions = list_sessions(runner)?;
    let session = find_session(&sessions, &current)
        .ok_or_else(|| anyhow!("current session not found: {current}"))?;
    let category = resolve_category_for_session(config, session);
    let category_sessions = sessions_in_category(config, &sessions, &category);
    if category_sessions.is_empty() {
        bail!("no session in current category: {category}");
    }
    let index = category_sessions
        .iter()
        .position(|session| session.name == current)
        .unwrap_or(0);
    let next = match direction {
        Direction::Next => (index + 1) % category_sessions.len(),
        Direction::Previous => (index + category_sessions.len() - 1) % category_sessions.len(),
    };
    let next_name = category_sessions[next].name.clone();
    switch_client_for_client(runner, &client, &next_name)?;
    remember_session_for_client(runner, &client, &category, &next_name)
}

pub fn on_client_session_changed(
    runner: &dyn TmuxRunner,
    config: &Config,
    client_pid: Option<u32>,
    session_name: Option<&str>,
) -> Result<()> {
    let (client_name, session_name) = match (client_pid, session_name) {
        (Some(client_pid), Some(session_name)) => (
            regular_client_name_for_pid(runner, client_pid)?,
            session_name.to_string(),
        ),
        _ => (current_client_name(runner)?, current_session_name(runner)?),
    };
    remember_client_session_for_session(runner, config, &client_name, &session_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::mock::MockTmuxRunner;

    fn config_for_sessions(pairs: &[(&str, &str)]) -> crate::config::Config {
        let mut config = crate::config::Config::default();
        config.categories.session_name_rules = pairs
            .iter()
            .map(|(name, category)| crate::config::SessionNameRule {
                category: (*category).to_string(),
                patterns: vec![(*name).to_string()],
            })
            .collect();
        config
    }

    #[test]
    fn parse_sessions_reads_project_metadata_without_category_options() {
        let sep = '\u{1f}'.to_string();
        let raw = format!(
            "{}\n{}\n",
            ["main", "1", "100", "", "/repo", "", "$1"].join(&sep),
            ["sub", "0", "90", "", "", "", "$2"].join(&sep)
        );
        let sessions = parse_sessions(&raw);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].name, "main");
        assert!(sessions[0].attached);
        assert_eq!(sessions[0].project_path, "/repo");
    }

    #[test]
    fn list_sessions_uses_single_tmux_call() {
        let mock = MockTmuxRunner::new();
        let format = session_list_format();
        mock.stub(
            &["list-sessions", "-F", &format],
            "main\u{1f}1\u{1f}100\u{1f}\u{1f}\u{1f}\u{1f}$1\n",
        );
        let sessions = list_sessions(&mock).unwrap();
        assert_eq!(sessions[0].name, "main");
        assert_eq!(mock.calls().len(), 1);
    }

    #[test]
    fn current_context_reads_client_and_session() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["display-message", "-p", "#{client_name}\t#{client_tty}"],
            "client-1\t/dev/ttys001\n",
        );
        mock.stub(&["display-message", "-p", "#{session_name}"], "main\n");
        mock.stub(&["display-message", "-p", "#{session_id}"], "$3\n");
        assert_eq!(current_client_name(&mock).unwrap(), "client-1");
        assert_eq!(current_session_name(&mock).unwrap(), "main");
        assert_eq!(current_session_id(&mock).unwrap(), "$3");
    }

    #[test]
    fn current_session_id_rejects_non_stable_tmux_targets() {
        let mock = MockTmuxRunner::new();
        mock.stub(&["display-message", "-p", "#{session_id}"], "main\n");

        assert!(
            current_session_id(&mock)
                .unwrap_err()
                .to_string()
                .contains("valid current session ID")
        );
    }

    #[test]
    fn client_session_context_pins_one_regular_client_and_source_session_in_one_query() {
        let mock = MockTmuxRunner::new();
        let format = client_session_context_format();
        let sep = FIELD_SEP;
        mock.stub(
            &["list-clients", "-F", &format],
            &format!(
                "one{sep}/dev/ttys001{sep}$1{sep}%1{sep}0\n\
                 two{sep}/dev/ttys002{sep}$2{sep}%2{sep}0\n\
                 control{sep}{sep}$3{sep}%1{sep}1\n"
            ),
        );

        assert_eq!(
            client_session_context_for_pane(&mock, "%1", None).unwrap(),
            ClientSessionContext {
                client_name: "one".to_string(),
                session_id: "$1".to_string(),
            }
        );
        assert_eq!(mock.calls().len(), 1);
    }

    #[test]
    fn client_session_context_uses_the_only_regular_client_when_run_shell_keeps_a_stale_pane() {
        let mock = MockTmuxRunner::new();
        let format = client_session_context_format();
        let sep = FIELD_SEP;
        mock.stub(
            &["list-clients", "-F", &format],
            &format!("one{sep}/dev/ttys001{sep}$1{sep}%1{sep}0\n"),
        );

        assert_eq!(
            client_session_context_for_pane(&mock, "%999", None).unwrap(),
            ClientSessionContext {
                client_name: "one".to_string(),
                session_id: "$1".to_string(),
            }
        );
    }

    #[test]
    fn client_session_context_fails_closed_when_the_invoking_pane_is_ambiguous() {
        let mock = MockTmuxRunner::new();
        let format = client_session_context_format();
        let sep = FIELD_SEP;
        mock.stub(
            &["list-clients", "-F", &format],
            &format!(
                "one{sep}/dev/ttys001{sep}$1{sep}%1{sep}0\n\
                 two{sep}/dev/ttys002{sep}$2{sep}%1{sep}0\n"
            ),
        );

        assert!(
            client_session_context_for_pane(&mock, "%1", None)
                .unwrap_err()
                .to_string()
                .contains("multiple tmux clients")
        );
        assert!(
            client_session_context_for_pane(&mock, "%9", None)
                .unwrap_err()
                .to_string()
                .contains("does not identify one regular tmux client")
        );
    }

    #[test]
    fn client_session_context_uses_an_explicit_client_when_the_pane_is_shared() {
        let mock = MockTmuxRunner::new();
        let format = client_session_context_format();
        let sep = FIELD_SEP;
        mock.stub(
            &["list-clients", "-F", &format],
            &format!(
                "one{sep}/dev/ttys001{sep}$1{sep}%1{sep}0\n\
                 two{sep}/dev/ttys002{sep}$2{sep}%1{sep}0\n"
            ),
        );

        assert_eq!(
            client_session_context_for_pane(&mock, "%1", Some("two")).unwrap(),
            ClientSessionContext {
                client_name: "two".to_string(),
                session_id: "$2".to_string(),
            }
        );
    }

    #[test]
    fn current_client_name_falls_back_to_list_clients_when_tmux_has_no_current_client() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["display-message", "-p", "#{client_name}\t#{client_tty}"],
            "\t\n",
        );
        mock.stub(
            &["list-clients", "-F", "#{client_name}\t#{client_tty}"],
            "\t/dev/ttys001\n",
        );

        assert_eq!(current_client_name(&mock).unwrap(), "/dev/ttys001");
    }

    #[test]
    fn current_client_name_falls_back_to_list_clients_when_display_message_fails() {
        let mock = MockTmuxRunner::new();
        mock.stub(
            &["list-clients", "-F", "#{client_name}\t#{client_tty}"],
            "abc\t/dev/ttys001\n",
        );

        assert_eq!(current_client_name(&mock).unwrap(), "abc");
    }

    #[test]
    fn client_memory_key_hex_encodes_client_name() {
        assert_eq!(client_memory_key("abc", "work"), "@vde_client_616263_work");
        assert_eq!(
            client_memory_key("a/b", "work/private"),
            "@vde_client_612f62_work_private"
        );
    }

    #[test]
    fn remember_current_client_session_uses_effective_category() {
        let mock = MockTmuxRunner::new();
        let format = session_list_format();
        mock.stub(
            &["display-message", "-p", "#{client_name}\t#{client_tty}"],
            "abc\t/dev/ttys001\n",
        );
        mock.stub(&["display-message", "-p", "#{session_name}"], "main\n");
        mock.stub(
            &["list-sessions", "-F", &format],
            "main\u{1f}1\u{1f}100\u{1f}work\u{1f}\u{1f}\u{1f}$1\n",
        );
        mock.stub(&["set-option", "-g", "@vde_client_616263_work", "main"], "");
        remember_current_client_session(&mock, &config_for_sessions(&[("main", "work")])).unwrap();
        assert_eq!(mock.calls().len(), 4);
    }

    #[test]
    fn create_session_sets_project_category_switches_and_remembers() {
        let mock = MockTmuxRunner::new();
        let mut config = crate::config::Config::default();
        config.categories.default_category = Some("public".to_string());
        mock.stub(
            &["display-message", "-p", "#{client_name}\t#{client_tty}"],
            "client\t/dev/ttys001\n",
        );
        mock.stub(
            &[
                "new-session",
                "-d",
                "-P",
                "-F",
                "#{session_name}\u{1f}#{window_id}",
                "-c",
                "/Users/me",
            ],
            "zsh\u{1f}@9\n",
        );
        mock.stub(
            &[
                "set-option",
                "-t",
                "zsh",
                crate::options::KEY_PROJECT_PATH,
                "/Users/me",
            ],
            "",
        );
        mock.stub(
            &[
                "set-option",
                "-t",
                "zsh",
                crate::options::KEY_CATEGORY,
                "public",
            ],
            "",
        );
        mock.stub(&["switch-client", "-c", "client", "-t", "=zsh:"], "");
        mock.stub(
            &["show-hooks", "-g", "after-new-window[90]"],
            "after-new-window[90] \n",
        );
        mock.stub(
            &["set-option", "-g", "@vde_client_636c69656e74_public", "zsh"],
            "",
        );

        let session = create_session(
            &mock,
            &config,
            &BTreeMap::from([("HOME".to_string(), "/Users/me".to_string())]),
            Some("~/"),
        )
        .unwrap();

        assert_eq!(session, "zsh");
        assert_eq!(mock.calls().len(), 7);
    }

    #[test]
    fn create_session_opens_sidebar_for_created_window_when_all_sidebar_is_enabled() {
        let mock = MockTmuxRunner::new();
        let exe = std::env::current_exe().unwrap();
        let attach_command = format!(
            "{} sidebar attach",
            shell_quote_for_test(&exe.display().to_string())
        );
        let mut config = crate::config::Config::default();
        config.categories.default_category = Some("public".to_string());
        config.sidebar.width = crate::config::SidebarWidth::Columns(40);
        mock.stub(
            &["display-message", "-p", "#{client_name}\t#{client_tty}"],
            "client\t/dev/ttys001\n",
        );
        mock.stub(
            &[
                "new-session",
                "-d",
                "-P",
                "-F",
                "#{session_name}\u{1f}#{window_id}",
                "-c",
                "/Users/me",
            ],
            "zsh\u{1f}@9\n",
        );
        mock.stub(
            &[
                "set-option",
                "-t",
                "zsh",
                crate::options::KEY_PROJECT_PATH,
                "/Users/me",
            ],
            "",
        );
        mock.stub(
            &[
                "set-option",
                "-t",
                "zsh",
                crate::options::KEY_CATEGORY,
                "public",
            ],
            "",
        );
        mock.stub(&["switch-client", "-c", "client", "-t", "=zsh:"], "");
        mock.stub(
            &["show-hooks", "-g", "after-new-window[90]"],
            "after-new-window[90] run-shell 'vt sidebar layout-applied'\n",
        );
        mock.stub(
            &[
                "list-panes",
                "-t",
                "@9",
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
                "@9",
                "-F",
                "#{window_layout}",
            ],
            "layout-before\n",
        );
        mock.stub(
            &[
                "split-window",
                "-d",
                "-t",
                "@9",
                "-hbf",
                "-l",
                "40",
                &attach_command,
            ],
            "",
        );
        mock.stub(
            &["set-option", "-g", "@vde_client_636c69656e74_public", "zsh"],
            "",
        );

        create_session(
            &mock,
            &config,
            &BTreeMap::from([("HOME".to_string(), "/Users/me".to_string())]),
            Some("~/"),
        )
        .unwrap();

        assert!(mock.calls().iter().any(|call| {
            call.first().map(String::as_str) == Some("split-window")
                && call.get(2).map(String::as_str) == Some("-t")
                && call.get(3).map(String::as_str) == Some("@9")
        }));
        let calls = mock.calls();
        let switch_index = calls
            .iter()
            .position(|call| call.first().map(String::as_str) == Some("switch-client"))
            .unwrap();
        let split_index = calls
            .iter()
            .position(|call| call.first().map(String::as_str) == Some("split-window"))
            .unwrap();
        assert!(
            switch_index < split_index,
            "sidebar should open after the created session is attached"
        );
    }

    #[test]
    fn cycle_session_switches_next_in_current_category() {
        let mock = MockTmuxRunner::new();
        let format = session_list_format();
        mock.stub(
            &["display-message", "-p", "#{client_name}\t#{client_tty}"],
            "abc\t/dev/ttys001\n",
        );
        mock.stub(&["display-message", "-p", "#{session_name}"], "main\n");
        mock.stub(
            &["list-sessions", "-F", &format],
            "main\u{1f}1\u{1f}100\u{1f}work\u{1f}\u{1f}\u{1f}$1\nsub\u{1f}0\u{1f}101\u{1f}work\u{1f}\u{1f}\u{1f}$2\nother\u{1f}0\u{1f}102\u{1f}private\u{1f}\u{1f}\u{1f}$3\n",
        );
        mock.stub(&["switch-client", "-c", "abc", "-t", "=sub:"], "");
        mock.stub(&["set-option", "-g", "@vde_client_616263_work", "sub"], "");
        cycle_session(
            &mock,
            &config_for_sessions(&[("main", "work"), ("sub", "work"), ("other", "private")]),
            Direction::Next,
        )
        .unwrap();
        assert_eq!(mock.calls().len(), 5);
    }

    #[test]
    fn use_adjacent_category_switches_with_explicit_client_and_stops_after_switch() {
        let mock = MockTmuxRunner::new();
        let format = session_list_format();
        mock.stub(&["display-message", "-p", "#{session_name}"], "main\n");
        mock.stub(
            &["list-sessions", "-F", &format],
            "main\u{1f}1\u{1f}100\u{1f}a\u{1f}\u{1f}\u{1f}$1\none\u{1f}0\u{1f}101\u{1f}b\u{1f}\u{1f}\u{1f}$2\n",
        );
        mock.stub(
            &["display-message", "-p", "#{client_name}\t#{client_tty}"],
            "abc\t/dev/ttys001\n",
        );
        mock.stub(&["show-option", "-gqv", "@vde_client_616263_b"], "");
        mock.stub(&["switch-client", "-c", "abc", "-t", "=one:"], "");
        mock.stub(&["set-option", "-g", "@vde_client_616263_b", "one"], "");

        use_adjacent_category(
            &mock,
            &config_for_sessions(&[("main", "a"), ("one", "b")]),
            Direction::Next,
        )
        .unwrap();

        let calls = mock.calls();
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.first().map(String::as_str) == Some("list-sessions"))
                .count(),
            1
        );
        assert_eq!(
            calls.last().unwrap(),
            &["switch-client", "-c", "abc", "-t", "=one:"]
        );
    }

    #[test]
    fn use_category_prefers_remembered_session_and_stops_after_switch() {
        let mock = MockTmuxRunner::new();
        let format = session_list_format();
        mock.stub(
            &["display-message", "-p", "#{client_name}\t#{client_tty}"],
            "abc\t/dev/ttys001\n",
        );
        mock.stub(
            &["list-sessions", "-F", &format],
            "main\u{1f}1\u{1f}100\u{1f}work\u{1f}\u{1f}\u{1f}$1\nsub\u{1f}0\u{1f}101\u{1f}work\u{1f}\u{1f}\u{1f}$2\n",
        );
        mock.stub(&["show-option", "-gqv", "@vde_client_616263_work"], "sub\n");
        mock.stub(&["switch-client", "-c", "abc", "-t", "=sub:"], "");
        mock.stub(&["set-option", "-g", "@vde_client_616263_work", "sub"], "");
        use_category(
            &mock,
            &config_for_sessions(&[("main", "work"), ("sub", "work")]),
            "work",
        )
        .unwrap();
        let calls = mock.calls();
        assert_eq!(
            calls.last().unwrap(),
            &["switch-client", "-c", "abc", "-t", "=sub:"]
        );
        assert!(calls.iter().all(|call| {
            !(call.first().map(String::as_str) == Some("set-option")
                && call.get(2).map(String::as_str) == Some("@vde_client_616263_work"))
        }));
    }

    #[test]
    fn hook_with_args_remembers_given_client_session() {
        let mock = MockTmuxRunner::new();
        let client_format = client_pid_name_format();
        let session_format = session_list_format();
        mock.stub(
            &["list-clients", "-F", &client_format],
            "123\u{1f}abc\u{1f}/dev/ttys001\u{1f}0\n",
        );
        mock.stub(
            &["list-sessions", "-F", &session_format],
            "main\u{1f}1\u{1f}100\u{1f}work\u{1f}\u{1f}\u{1f}$1\n",
        );
        mock.stub(&["set-option", "-g", "@vde_client_616263_work", "main"], "");
        on_client_session_changed(
            &mock,
            &config_for_sessions(&[("main", "work")]),
            Some(123),
            Some("main"),
        )
        .unwrap();
        assert_eq!(mock.calls().len(), 3);
    }

    #[test]
    fn hook_resolves_session_name_rule_when_stored_category_is_empty() {
        let mock = MockTmuxRunner::new();
        let client_format = client_pid_name_format();
        let session_format = session_list_format();
        mock.stub(
            &["list-clients", "-F", &client_format],
            "123\u{1f}abc\u{1f}/dev/ttys001\u{1f}0\n",
        );
        mock.stub(
            &["list-sessions", "-F", &session_format],
            "dotfiles\u{1f}1\u{1f}100\u{1f}\u{1f}\u{1f}\u{1f}$1\n",
        );
        mock.stub(
            &["set-option", "-g", "@vde_client_616263_private", "dotfiles"],
            "",
        );
        let mut config = crate::config::Config::default();
        config
            .categories
            .session_name_rules
            .push(crate::config::SessionNameRule {
                category: "private".to_string(),
                patterns: vec!["dotfiles".to_string()],
            });

        on_client_session_changed(&mock, &config, Some(123), Some("dotfiles")).unwrap();

        assert_eq!(
            mock.calls().last().unwrap(),
            &["set-option", "-g", "@vde_client_616263_private", "dotfiles",]
        );
        assert!(mock.calls().iter().all(|call| {
            call.as_slice() != ["set-option", "-g", "@vde_client_616263_", "dotfiles"]
        }));
    }

    #[test]
    fn client_pid_resolution_is_exact_and_fail_closed() {
        let format = client_pid_name_format();

        let valid = MockTmuxRunner::new();
        valid.stub(
            &["list-clients", "-F", &format],
            "122\u{1f}other\u{1f}/dev/ttys000\u{1f}0\n123\u{1f}\u{1f}/dev/ttys001\u{1f}0\n",
        );
        assert_eq!(
            regular_client_name_for_pid(&valid, 123).unwrap(),
            "/dev/ttys001"
        );

        let detached = MockTmuxRunner::new();
        detached.stub(&["list-clients", "-F", &format], "");
        assert!(
            regular_client_name_for_pid(&detached, 123)
                .unwrap_err()
                .to_string()
                .contains("not attached")
        );

        let control = MockTmuxRunner::new();
        control.stub(
            &["list-clients", "-F", &format],
            "123\u{1f}control\u{1f}\u{1f}1\n",
        );
        assert!(regular_client_name_for_pid(&control, 123).is_err());

        let duplicate = MockTmuxRunner::new();
        duplicate.stub(
            &["list-clients", "-F", &format],
            "123\u{1f}one\u{1f}/dev/ttys001\u{1f}0\n123\u{1f}two\u{1f}/dev/ttys002\u{1f}0\n",
        );
        assert!(
            regular_client_name_for_pid(&duplicate, 123)
                .unwrap_err()
                .to_string()
                .contains("multiple")
        );

        let invalid = MockTmuxRunner::new();
        assert!(regular_client_name_for_pid(&invalid, 0).is_err());
        assert!(invalid.calls().is_empty());
    }

    #[test]
    fn sync_session_category_mirrors_sets_effective_categories() {
        let mock = MockTmuxRunner::new();
        let format = session_list_format();
        mock.stub(
            &["list-sessions", "-F", &format],
            "main\u{1f}1\u{1f}100\u{1f}\u{1f}\u{1f}\u{1f}$1\n",
        );
        mock.stub(
            &["set-option", "-t", "main", crate::options::KEY_CATEGORY, ""],
            "",
        );
        sync_session_category_mirrors(&mock, &crate::config::Config::default()).unwrap();
        assert_eq!(mock.calls().len(), 2);
    }

    #[test]
    fn sync_session_category_mirrors_uses_config_default() {
        let mock = MockTmuxRunner::new();
        let format = session_list_format();
        let mut config = crate::config::Config::default();
        config.categories.default_category = Some("public".to_string());
        mock.stub(
            &["list-sessions", "-F", &format],
            "main\u{1f}1\u{1f}100\u{1f}\u{1f}/Users/me\u{1f}\u{1f}$1\n",
        );
        mock.stub(
            &[
                "set-option",
                "-t",
                "main",
                crate::options::KEY_CATEGORY,
                "public",
            ],
            "",
        );
        sync_session_category_mirrors(&mock, &config).unwrap();
        assert_eq!(mock.calls().len(), 2);
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
}
