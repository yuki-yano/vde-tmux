use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::{Duration, Instant};

use crate::daemon::protocol::v2::SessionLinkPresentation;
use crate::pane_state::{EventId, PaneInstance};
use crate::tmux::{SystemTmuxRunner, TmuxRunner};

pub const TOPOLOGY_FIELD_COUNT: usize = 12;
pub const MAX_TOPOLOGY_ROWS: usize = 64;
pub const TARGETED_REFRESH_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerIdentity {
    pub pid: u32,
    pub start_time: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryFraming {
    token: String,
    field: String,
    row: String,
    header: String,
    session: String,
}

impl QueryFraming {
    pub fn generate() -> Result<Self, TopologyError> {
        let token = EventId::generate()
            .map_err(|error| TopologyError::InvalidFraming(error.to_string()))?
            .as_str()
            .to_string();
        Self::from_token(token)
    }

    pub fn from_token(token: impl Into<String>) -> Result<Self, TopologyError> {
        let token = token.into();
        if token.len() < 32
            || !token
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(TopologyError::InvalidFraming(
                "query sentinel must contain at least 128 bits of lowercase hex".to_string(),
            ));
        }
        Ok(Self {
            field: format!("__vde_f_{token}__"),
            row: format!("__vde_r_{token}__"),
            header: format!("__vde_h_{token}__"),
            session: format!("__vde_s_{token}__"),
            token,
        })
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn field_separator(&self) -> &str {
        &self.field
    }

    pub fn row_separator(&self) -> &str {
        &self.row
    }

    pub fn identity_format(&self) -> String {
        format!(
            "{}{}#{{pid}}{}#{{start_time}}{}",
            self.header, self.field, self.field, self.row
        )
    }

    pub fn pane_format(&self) -> String {
        const FIELDS: [&str; TOPOLOGY_FIELD_COUNT] = [
            "#{session_id}",
            "#{session_name}",
            "#{window_id}",
            "#{window_index}",
            "#{window_active}",
            "#{window_last_flag}",
            "#{window_name}",
            "#{pane_id}",
            "#{pane_pid}",
            "#{pane_current_path}",
            "#{pane_current_command}",
            "#{pane_active}",
        ];
        format!("{}{}", FIELDS.join(&self.field), self.row)
    }

    pub fn session_format(&self) -> String {
        format!("{}#{{session_id}}{}", self.session, self.row)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopologyPane {
    pub pane_instance: PaneInstance,
    pub session_links: Vec<SessionLinkPresentation>,
    pub window_id: String,
    pub window_name: String,
    pub current_path: String,
    pub current_command: String,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopologySnapshot {
    pub server_identity: ServerIdentity,
    pub panes: Vec<TopologyPane>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopologyError {
    InvalidFraming(String),
    IdentityMismatch {
        expected: ServerIdentity,
        actual: ServerIdentity,
    },
    InvalidRow(String),
    TooManyRows(usize),
    InvalidPaneId(String),
    Query(String),
    Deadline,
}

impl fmt::Display for TopologyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidFraming(message) | Self::InvalidRow(message) => {
                formatter.write_str(message)
            }
            Self::IdentityMismatch { expected, actual } => write!(
                formatter,
                "tmux server identity mismatch: expected {}:{}, received {}:{}",
                expected.pid, expected.start_time, actual.pid, actual.start_time
            ),
            Self::TooManyRows(count) => write!(formatter, "topology has too many rows: {count}"),
            Self::InvalidPaneId(pane_id) => write!(formatter, "invalid pane ID {pane_id:?}"),
            Self::Query(message) => formatter.write_str(message),
            Self::Deadline => formatter.write_str("targeted pane refresh deadline exceeded"),
        }
    }
}

pub trait TargetedRefreshIo {
    fn run(&self, args: &[String], timeout: Duration) -> Result<String, TopologyError>;
}

#[derive(Debug, Clone)]
pub struct SystemTargetedRefreshIo {
    socket_name: Option<String>,
}

impl SystemTargetedRefreshIo {
    pub fn new(socket_name: Option<String>) -> Self {
        Self { socket_name }
    }
}

impl TargetedRefreshIo for SystemTargetedRefreshIo {
    fn run(&self, args: &[String], timeout: Duration) -> Result<String, TopologyError> {
        let runner = match &self.socket_name {
            Some(socket_name) => {
                SystemTmuxRunner::with_socket_name(socket_name.clone(), Some(timeout))
            }
            None => SystemTmuxRunner::with_timeout(timeout),
        };
        let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
        runner
            .run(&refs)
            .map_err(|error| TopologyError::Query(error.to_string()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetedRefreshOutcome {
    Found(TopologyPane),
    NotFound,
}

pub fn targeted_refresh(
    io: &dyn TargetedRefreshIo,
    pane_id: &str,
    expected_identity: &ServerIdentity,
) -> Result<TargetedRefreshOutcome, TopologyError> {
    let sessions = QueryFraming::generate()?;
    let panes = QueryFraming::generate()?;
    targeted_refresh_with_framings(
        io,
        pane_id,
        expected_identity,
        &sessions,
        &panes,
        Instant::now() + TARGETED_REFRESH_TIMEOUT,
    )
}

fn targeted_refresh_with_framings(
    io: &dyn TargetedRefreshIo,
    pane_id: &str,
    expected_identity: &ServerIdentity,
    session_framing: &QueryFraming,
    pane_framing: &QueryFraming,
    deadline: Instant,
) -> Result<TargetedRefreshOutcome, TopologyError> {
    validate_pane_id(pane_id)?;
    let sessions = io.run(
        &targeted_session_query_args(session_framing),
        remaining(deadline)?,
    )?;
    if parse_session_count(&sessions, session_framing, expected_identity)? == 0 {
        return Ok(TargetedRefreshOutcome::NotFound);
    }
    let panes = io.run(
        &targeted_pane_query_args(pane_framing, pane_id)?,
        remaining(deadline)?,
    )?;
    let mut snapshot = parse_topology(&panes, pane_framing, expected_identity)?;
    match snapshot.panes.len() {
        0 => Ok(TargetedRefreshOutcome::NotFound),
        1 if snapshot.panes[0].pane_instance.pane_id == pane_id => {
            Ok(TargetedRefreshOutcome::Found(snapshot.panes.remove(0)))
        }
        _ => Err(TopologyError::InvalidRow(
            "targeted pane query returned unrelated panes".to_string(),
        )),
    }
}

fn remaining(deadline: Instant) -> Result<Duration, TopologyError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(TopologyError::Deadline)
}

impl std::error::Error for TopologyError {}

pub fn poll_query_args(framing: &QueryFraming) -> Vec<String> {
    vec![
        "display-message".to_string(),
        "-p".to_string(),
        framing.identity_format(),
        ";".to_string(),
        "list-panes".to_string(),
        "-a".to_string(),
        "-F".to_string(),
        framing.pane_format(),
    ]
}

pub fn targeted_session_query_args(framing: &QueryFraming) -> Vec<String> {
    vec![
        "display-message".to_string(),
        "-p".to_string(),
        framing.identity_format(),
        ";".to_string(),
        "list-sessions".to_string(),
        "-F".to_string(),
        framing.session_format(),
    ]
}

pub fn targeted_pane_query_args(
    framing: &QueryFraming,
    pane_id: &str,
) -> Result<Vec<String>, TopologyError> {
    validate_pane_id(pane_id)?;
    Ok(vec![
        "display-message".to_string(),
        "-p".to_string(),
        framing.identity_format(),
        ";".to_string(),
        "list-panes".to_string(),
        "-a".to_string(),
        "-f".to_string(),
        format!("#{{==:#{{pane_id}},{pane_id}}}"),
        "-F".to_string(),
        framing.pane_format(),
    ])
}

pub fn validate_pane_id(pane_id: &str) -> Result<(), TopologyError> {
    if pane_id.strip_prefix('%').is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        Ok(())
    } else {
        Err(TopologyError::InvalidPaneId(pane_id.to_string()))
    }
}

pub fn parse_session_count(
    output: &str,
    framing: &QueryFraming,
    expected_identity: &ServerIdentity,
) -> Result<usize, TopologyError> {
    let (identity, rows) = parse_envelope(output, framing)?;
    verify_identity(identity, expected_identity)?;
    let mut sessions = BTreeSet::new();
    for row in rows {
        let Some(session_id) = row.strip_prefix(&framing.session) else {
            return Err(TopologyError::InvalidRow(
                "session row has an invalid prefix".to_string(),
            ));
        };
        validate_prefixed_numeric_id(session_id, '$', "session ID")?;
        sessions.insert(session_id.to_string());
    }
    Ok(sessions.len())
}

pub fn parse_topology(
    output: &str,
    framing: &QueryFraming,
    expected_identity: &ServerIdentity,
) -> Result<TopologySnapshot, TopologyError> {
    let (identity, rows) = parse_envelope(output, framing)?;
    verify_identity(identity.clone(), expected_identity)?;
    let mut panes = BTreeMap::<PaneInstance, TopologyPane>::new();
    let mut pane_pids = BTreeMap::<String, u32>::new();
    for row in rows {
        let fields = row.split(&framing.field).collect::<Vec<_>>();
        if fields.len() != TOPOLOGY_FIELD_COUNT {
            return Err(TopologyError::InvalidRow(format!(
                "topology row has {} fields, expected {TOPOLOGY_FIELD_COUNT}",
                fields.len()
            )));
        }
        if fields.iter().any(|field| {
            field.contains(&framing.field)
                || field.contains(&framing.row)
                || field.contains(&framing.header)
        }) {
            return Err(TopologyError::InvalidRow(
                "topology value contains a query sentinel".to_string(),
            ));
        }
        validate_prefixed_numeric_id(fields[0], '$', "session ID")?;
        validate_prefixed_numeric_id(fields[2], '@', "window ID")?;
        validate_pane_id(fields[7])?;
        let window_index = parse_i64(fields[3], "window index")?;
        let window_active = parse_bool(fields[4], "window active")?;
        let window_last = parse_bool(fields[5], "window last flag")?;
        let pane_pid = fields[8]
            .parse::<u32>()
            .ok()
            .filter(|pid| *pid > 0)
            .ok_or_else(|| TopologyError::InvalidRow("invalid pane PID".to_string()))?;
        let active = parse_bool(fields[11], "pane active")?;
        let pane_instance = PaneInstance {
            pane_id: fields[7].to_string(),
            pane_pid,
        };
        if pane_pids
            .insert(pane_instance.pane_id.clone(), pane_pid)
            .is_some_and(|existing| existing != pane_pid)
        {
            return Err(TopologyError::InvalidRow(format!(
                "pane {} has multiple PIDs in one batch",
                pane_instance.pane_id
            )));
        }
        let session_link = SessionLinkPresentation {
            session_id: fields[0].to_string(),
            session_name: fields[1].to_string(),
            window_index,
            window_active,
            window_last,
        };
        let pane = TopologyPane {
            pane_instance: pane_instance.clone(),
            session_links: vec![session_link.clone()],
            window_id: fields[2].to_string(),
            window_name: fields[6].to_string(),
            current_path: fields[9].to_string(),
            current_command: fields[10].to_string(),
            active,
        };
        match panes.get_mut(&pane_instance) {
            Some(existing) => {
                if existing.window_id != pane.window_id
                    || existing.window_name != pane.window_name
                    || existing.current_path != pane.current_path
                    || existing.current_command != pane.current_command
                    || existing.active != pane.active
                {
                    return Err(TopologyError::InvalidRow(format!(
                        "linked rows disagree for pane {}",
                        pane_instance.pane_id
                    )));
                }
                if let Some(existing_link) = existing
                    .session_links
                    .iter()
                    .find(|link| link.session_id == session_link.session_id)
                {
                    if existing_link != &session_link {
                        return Err(TopologyError::InvalidRow(format!(
                            "linked rows disagree for session {}",
                            session_link.session_id
                        )));
                    }
                } else {
                    existing.session_links.push(session_link);
                }
            }
            None => {
                panes.insert(pane_instance, pane);
            }
        }
    }
    let mut panes = panes.into_values().collect::<Vec<_>>();
    for pane in &mut panes {
        pane.session_links
            .sort_by(|left, right| left.session_id.cmp(&right.session_id));
    }
    Ok(TopologySnapshot {
        server_identity: identity,
        panes,
    })
}

fn parse_envelope<'a>(
    output: &'a str,
    framing: &QueryFraming,
) -> Result<(ServerIdentity, Vec<&'a str>), TopologyError> {
    if !output.trim_end_matches('\n').ends_with(&framing.row) {
        return Err(TopologyError::InvalidFraming(
            "query output is missing the final row sentinel".to_string(),
        ));
    }
    let mut chunks = output.split(&framing.row);
    let header = chunks
        .next()
        .ok_or_else(|| TopologyError::InvalidFraming("identity header is missing".to_string()))?;
    let fields = header.split(&framing.field).collect::<Vec<_>>();
    if fields.len() != 3 || fields[0] != framing.header {
        return Err(TopologyError::InvalidFraming(
            "identity header prefix or field count is invalid".to_string(),
        ));
    }
    let identity = ServerIdentity {
        pid: fields[1]
            .parse::<u32>()
            .ok()
            .filter(|pid| *pid > 0)
            .ok_or_else(|| TopologyError::InvalidFraming("invalid server PID".to_string()))?,
        start_time: fields[2]
            .parse::<i64>()
            .map_err(|_| TopologyError::InvalidFraming("invalid server start time".to_string()))?,
    };
    let mut rows = Vec::new();
    for chunk in chunks {
        let chunk = chunk.strip_prefix('\n').unwrap_or(chunk);
        if chunk.is_empty() {
            continue;
        }
        if chunk.chars().all(|character| character == '\n') {
            continue;
        }
        rows.push(chunk);
        if rows.len() > MAX_TOPOLOGY_ROWS {
            return Err(TopologyError::TooManyRows(rows.len()));
        }
    }
    Ok((identity, rows))
}

fn verify_identity(actual: ServerIdentity, expected: &ServerIdentity) -> Result<(), TopologyError> {
    if &actual == expected {
        Ok(())
    } else {
        Err(TopologyError::IdentityMismatch {
            expected: expected.clone(),
            actual,
        })
    }
}

fn validate_prefixed_numeric_id(
    value: &str,
    prefix: char,
    field: &str,
) -> Result<(), TopologyError> {
    if value.strip_prefix(prefix).is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        Ok(())
    } else {
        Err(TopologyError::InvalidRow(format!("invalid {field}")))
    }
}

fn parse_i64(value: &str, field: &str) -> Result<i64, TopologyError> {
    value
        .parse()
        .map_err(|_| TopologyError::InvalidRow(format!("invalid {field}")))
}

fn parse_bool(value: &str, field: &str) -> Result<bool, TopologyError> {
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(TopologyError::InvalidRow(format!("invalid {field}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct FakeRefreshIo {
        outputs: Mutex<VecDeque<String>>,
        calls: Mutex<Vec<(Vec<String>, Duration)>>,
    }

    impl TargetedRefreshIo for FakeRefreshIo {
        fn run(&self, args: &[String], timeout: Duration) -> Result<String, TopologyError> {
            self.calls.lock().unwrap().push((args.to_vec(), timeout));
            self.outputs
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| TopologyError::Query("missing fake output".to_string()))
        }
    }

    fn framing() -> QueryFraming {
        QueryFraming::from_token("00112233445566778899aabbccddeeff").unwrap()
    }

    fn identity() -> ServerIdentity {
        ServerIdentity {
            pid: 123,
            start_time: 456,
        }
    }

    fn output(rows: &[Vec<&str>]) -> String {
        let framing = framing();
        let mut output = format!(
            "{}{}123{}456{}\n",
            framing.header, framing.field, framing.field, framing.row
        );
        for fields in rows {
            output.push_str(&fields.join(&framing.field));
            output.push_str(&framing.row);
            output.push('\n');
        }
        output
    }

    #[test]
    fn linked_window_rows_are_deduplicated_into_session_links() {
        let rows = vec![
            vec![
                "$1", "alpha", "@2", "1", "1", "0", "main", "%3", "99", "/tmp", "zsh", "1",
            ],
            vec![
                "$2", "beta", "@2", "4", "0", "1", "main", "%3", "99", "/tmp", "zsh", "1",
            ],
        ];
        let snapshot = parse_topology(&output(&rows), &framing(), &identity()).unwrap();
        assert_eq!(snapshot.panes.len(), 1);
        assert_eq!(snapshot.panes[0].session_links.len(), 2);
        assert_eq!(snapshot.panes[0].session_links[0].session_id, "$1");
        assert_eq!(snapshot.panes[0].session_links[1].session_id, "$2");
    }

    #[test]
    fn topology_values_preserve_tabs_and_newlines() {
        let rows = vec![vec![
            "$1",
            "alpha\nteam",
            "@2",
            "1",
            "1",
            "0",
            "main\twork",
            "%3",
            "99",
            "/tmp/line\none",
            "zsh\tinteractive",
            "1",
        ]];
        let snapshot = parse_topology(&output(&rows), &framing(), &identity()).unwrap();
        assert_eq!(
            snapshot.panes[0].session_links[0].session_name,
            "alpha\nteam"
        );
        assert_eq!(snapshot.panes[0].window_name, "main\twork");
        assert_eq!(snapshot.panes[0].current_path, "/tmp/line\none");
        assert_eq!(snapshot.panes[0].current_command, "zsh\tinteractive");
    }

    #[test]
    fn zero_session_header_is_a_valid_empty_topology() {
        let snapshot = parse_topology(&output(&[]), &framing(), &identity()).unwrap();
        assert!(snapshot.panes.is_empty());
        let sessions = format!(
            "{}{}123{}456{}\n",
            framing().header,
            framing().field,
            framing().field,
            framing().row
        );
        assert_eq!(
            parse_session_count(&sessions, &framing(), &identity()).unwrap(),
            0
        );
    }

    #[test]
    fn invalid_fields_collision_and_identity_mismatch_reject_entire_batch() {
        let malformed = output(&[vec!["$1", "too", "few"]]);
        assert!(matches!(
            parse_topology(&malformed, &framing(), &identity()),
            Err(TopologyError::InvalidRow(_))
        ));

        let collision = output(&[vec![
            "$1",
            "alpha",
            "@2",
            "1",
            "1",
            "0",
            "main",
            "%3",
            "99",
            "/tmp",
            "zsh",
            &format!("1{}collision", framing().row),
        ]]);
        assert!(parse_topology(&collision, &framing(), &identity()).is_err());

        let wrong = ServerIdentity {
            pid: 999,
            start_time: 456,
        };
        assert!(matches!(
            parse_topology(&output(&[]), &framing(), &wrong),
            Err(TopologyError::IdentityMismatch { .. })
        ));

        let truncated = output(&[vec![
            "$1", "alpha", "@2", "1", "1", "0", "main", "%3", "99", "/tmp", "zsh", "1",
        ]]);
        let suffix = format!("{}\n", framing().row);
        let truncated = format!("{}\n", truncated.strip_suffix(&suffix).unwrap());
        assert!(parse_topology(&truncated, &framing(), &identity()).is_err());
    }

    #[test]
    fn targeted_query_validates_id_and_uses_server_wide_filter() {
        let args = targeted_pane_query_args(&framing(), "%42").unwrap();
        assert!(args.iter().any(|arg| arg == "list-panes"));
        assert!(args.iter().any(|arg| arg == "-a"));
        assert!(args.iter().any(|arg| arg == "#{==:#{pane_id},%42}"));
        assert!(targeted_pane_query_args(&framing(), "%1},#{pid}").is_err());
    }

    #[test]
    fn row_limit_is_all_or_nothing() {
        let row = vec![
            "$1", "alpha", "@2", "1", "1", "0", "main", "%3", "99", "/tmp", "zsh", "1",
        ];
        let rows = std::iter::repeat_n(row, MAX_TOPOLOGY_ROWS + 1).collect::<Vec<_>>();
        assert!(matches!(
            parse_topology(&output(&rows), &framing(), &identity()),
            Err(TopologyError::TooManyRows(_))
        ));
    }

    #[test]
    fn targeted_refresh_skips_pane_query_for_zero_session_server() {
        let framing = framing();
        let io = FakeRefreshIo {
            outputs: Mutex::new(VecDeque::from([output(&[])])),
            calls: Mutex::new(Vec::new()),
        };
        let result = targeted_refresh_with_framings(
            &io,
            "%1",
            &identity(),
            &framing,
            &framing,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(result, TargetedRefreshOutcome::NotFound);
        assert_eq!(io.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn targeted_refresh_uses_two_processes_and_keeps_linked_rows() {
        let framing = framing();
        let sessions = format!(
            "{}{}123{}456{}\n{}$1{}\n",
            framing.header, framing.field, framing.field, framing.row, framing.session, framing.row,
        );
        let pane_rows = vec![
            vec![
                "$1", "alpha", "@2", "1", "1", "0", "main", "%3", "99", "/tmp", "zsh", "1",
            ],
            vec![
                "$2", "beta", "@2", "4", "0", "1", "main", "%3", "99", "/tmp", "zsh", "1",
            ],
        ];
        let io = FakeRefreshIo {
            outputs: Mutex::new(VecDeque::from([sessions, output(&pane_rows)])),
            calls: Mutex::new(Vec::new()),
        };
        let result = targeted_refresh_with_framings(
            &io,
            "%3",
            &identity(),
            &framing,
            &framing,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
        let TargetedRefreshOutcome::Found(pane) = result else {
            panic!("expected target pane");
        };
        assert_eq!(pane.session_links.len(), 2);
        let calls = io.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].1 > Duration::ZERO);
        assert!(calls[1].1 > Duration::ZERO);
        assert!(calls[1].1 <= calls[0].1);
    }
}
