//! Incremental JSONL structural reader. Body strings are validated and discarded byte by byte.
//! A cursor can stop inside an arbitrarily large record without retaining that record.
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::time::Instant;

use super::ingress::TranscriptLocator;

pub const SLICE_BYTES: usize = 4 * 1024 * 1024;
pub const REQUEST_BYTES: usize = 32 * 1024 * 1024;
const MAX_TURNS: usize = 4096;
const MAX_DEPTH: usize = 64;
const MAX_METADATA: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadFailure {
    HistoryUnknown,
    Unavailable,
}
type Result<T> = std::result::Result<T, ReadFailure>;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TurnOrder {
    pub start: u64,
    pub complete: Option<u64>,
    pub aborted: Option<u64>,
    pub error: bool,
}

#[derive(Debug, Clone)]
pub struct Cursor {
    locator: TranscriptLocator,
    session_digest: String,
    pub offset: u64,
    ordinal: u64,
    header: bool,
    unknown: bool,
    size_seen: u64,
    parser: Parser,
    turns: BTreeMap<String, TurnOrder>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceResult {
    pub bytes: usize,
    pub more: bool,
    pub partial: bool,
}

impl Cursor {
    pub fn new(locator: TranscriptLocator, session_digest: String) -> Self {
        Self {
            locator,
            session_digest,
            offset: 0,
            ordinal: 0,
            header: false,
            unknown: false,
            size_seen: 0,
            parser: Parser::default(),
            turns: BTreeMap::new(),
        }
    }

    pub fn header_verified(&self) -> bool {
        self.header && !self.unknown
    }
    pub fn locator(&self) -> &TranscriptLocator {
        &self.locator
    }
    pub fn proves(&self, issued: &str, submitted: &str) -> bool {
        if !self.header_verified() || issued == submitted {
            return false;
        }
        self.turns
            .get(issued)
            .zip(self.turns.get(submitted))
            .is_some_and(|(a, b)| {
                !a.error
                    && a.aborted.is_none()
                    && a.complete
                        .is_some_and(|complete| a.start < complete && complete < b.start)
            })
    }

    pub fn read_slice(&mut self, budget: usize, deadline: Instant) -> Result<SliceResult> {
        if self.unknown {
            return Err(ReadFailure::HistoryUnknown);
        }
        let result = self.read_slice_inner(budget.min(SLICE_BYTES), deadline);
        if result == Err(ReadFailure::HistoryUnknown) {
            self.unknown = true;
        }
        result
    }

    fn read_slice_inner(&mut self, budget: usize, deadline: Instant) -> Result<SliceResult> {
        if !self.locator.matches_current_file() {
            return Err(ReadFailure::HistoryUnknown);
        }
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&self.locator.transcript)
            .map_err(|_| ReadFailure::Unavailable)?;
        let before = file.metadata().map_err(|_| ReadFailure::Unavailable)?;
        if !before.is_file()
            || before.dev() != self.locator.dev
            || before.ino() != self.locator.ino
            || before.len() < self.size_seen
            || before.len() < self.offset
        {
            return Err(ReadFailure::HistoryUnknown);
        }
        file.seek(SeekFrom::Start(self.offset))
            .map_err(|_| ReadFailure::Unavailable)?;
        let mut buffer = [0_u8; 64 * 1024];
        let mut read = 0;
        while read < budget && Instant::now() < deadline {
            let count = file
                .read(&mut buffer[..(budget - read).min(64 * 1024)])
                .map_err(|_| ReadFailure::Unavailable)?;
            if count == 0 {
                break;
            }
            let mut position = 0;
            while position < count {
                let skipped = self.parser.skip_ignored_ascii(&buffer[position..count]);
                if skipped != 0 {
                    position += skipped;
                    self.offset += skipped as u64;
                    continue;
                }
                if let Some(record) = self.parser.feed(buffer[position])? {
                    self.apply_record(record)?;
                }
                position += 1;
                self.offset += 1;
            }
            read += count;
        }
        let after = file.metadata().map_err(|_| ReadFailure::Unavailable)?;
        if after.dev() != before.dev()
            || after.ino() != before.ino()
            || after.len() < before.len()
            || after.len() < self.offset
            || !self.locator.matches_current_file()
        {
            return Err(ReadFailure::HistoryUnknown);
        }
        self.size_seen = after.len();
        Ok(SliceResult {
            bytes: read,
            more: self.offset < after.len(),
            partial: !self.parser.empty(),
        })
    }

    fn apply_record(&mut self, record: Record) -> Result<()> {
        self.ordinal = self
            .ordinal
            .checked_add(1)
            .ok_or(ReadFailure::HistoryUnknown)?;
        if !self.header {
            if self.ordinal != 1
                || record.kind != Kind::SessionMeta
                || record.id.as_ref() != Some(&self.session_digest)
                || !record.user_source
                || record.parent
            {
                return Err(ReadFailure::HistoryUnknown);
            }
            self.header = true;
            return Ok(());
        }
        if record.kind == Kind::SessionMeta {
            return Err(ReadFailure::HistoryUnknown);
        }
        if record.kind != Kind::Event {
            return Ok(());
        }
        if record.event == Kind::Rollback {
            return Err(ReadFailure::HistoryUnknown);
        }
        if !matches!(record.event, Kind::Start | Kind::Complete | Kind::Abort) {
            return Ok(());
        }
        let id = record.turn.ok_or(ReadFailure::HistoryUnknown)?;
        if record.event == Kind::Start {
            if self.turns.contains_key(&id) || self.turns.len() >= MAX_TURNS {
                return Err(ReadFailure::HistoryUnknown);
            }
            self.turns.insert(
                id,
                TurnOrder {
                    start: self.ordinal,
                    ..TurnOrder::default()
                },
            );
        } else {
            let turn = self.turns.get_mut(&id).ok_or(ReadFailure::HistoryUnknown)?;
            if turn.complete.is_some() || turn.aborted.is_some() {
                return Err(ReadFailure::HistoryUnknown);
            }
            if record.event == Kind::Complete {
                turn.complete = Some(self.ordinal);
                turn.error = record.error;
            } else {
                turn.aborted = Some(self.ordinal);
            }
        }
        Ok(())
    }
}

pub fn identifier_digest(value: &str) -> String {
    super::digest(&Some(value))
}

/// The startup hook verifies only the header, with no filename search or body retention.
pub fn verify_startup_header(
    locator: &TranscriptLocator,
    session: &str,
    deadline: Instant,
) -> bool {
    let verify = || -> Result<bool> {
        if !locator.matches_current_file() {
            return Ok(false);
        }
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&locator.transcript)
            .map_err(|_| ReadFailure::Unavailable)?;
        let before = file.metadata().map_err(|_| ReadFailure::Unavailable)?;
        if before.dev() != locator.dev || before.ino() != locator.ino {
            return Ok(false);
        }
        let mut parser = Parser::default();
        let mut bytes = 0;
        let mut buffer = [0; 64 * 1024];
        while bytes < REQUEST_BYTES && Instant::now() < deadline {
            let count = file
                .read(&mut buffer)
                .map_err(|_| ReadFailure::Unavailable)?;
            if count == 0 {
                return Ok(false);
            }
            bytes += count;
            for byte in &buffer[..count] {
                if let Some(record) = parser.feed(*byte)? {
                    let after = file.metadata().map_err(|_| ReadFailure::Unavailable)?;
                    return Ok(record.kind == Kind::SessionMeta
                        && record.id.as_deref() == Some(session)
                        && record.user_source
                        && !record.parent
                        && after.len() >= before.len()
                        && after.dev() == before.dev()
                        && after.ino() == before.ino()
                        && locator.matches_current_file());
                }
            }
        }
        Ok(false)
    };
    verify().unwrap_or(false)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Kind {
    #[default]
    Other,
    SessionMeta,
    Event,
    Start,
    Complete,
    Abort,
    Rollback,
}
impl Kind {
    fn parse(value: &str) -> Self {
        match value {
            "session_meta" => Self::SessionMeta,
            "event_msg" => Self::Event,
            "task_started" | "turn_started" => Self::Start,
            "task_complete" | "turn_complete" => Self::Complete,
            "turn_aborted" => Self::Abort,
            "thread_rolled_back" => Self::Rollback,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Record {
    kind: Kind,
    event: Kind,
    id: Option<String>,
    turn: Option<String>,
    user_source: bool,
    parent: bool,
    error: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Field {
    #[default]
    Other,
    Type,
    Payload,
    Id,
    Turn,
    Source,
    Parent,
    Error,
}
impl Field {
    fn parse(value: &str) -> Self {
        match value {
            "type" => Self::Type,
            "payload" => Self::Payload,
            "id" => Self::Id,
            "turn_id" => Self::Turn,
            "thread_source" => Self::Source,
            "parent_thread_id" => Self::Parent,
            "error" => Self::Error,
            _ => Self::Other,
        }
    }
    fn bit(self) -> u16 {
        if self == Self::Other {
            0
        } else {
            1 << self as u16
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Root,
    Payload,
    Other,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    KeyOrEnd,
    Key,
    Colon,
    Value,
    ValueOrEnd,
    CommaObject,
    CommaArray,
}
#[derive(Debug, Clone)]
struct Frame {
    role: Role,
    stage: Stage,
    field: Field,
    seen: u16,
    array: bool,
}
#[derive(Debug, Clone, Copy)]
enum Target {
    Key,
    Value(Role, Field),
}

#[derive(Debug, Clone)]
enum Token {
    String {
        target: Target,
        bytes: Vec<u8>,
        keep: bool,
        escape: bool,
        unicode: u8,
        utf8: u8,
        low: u8,
        high: u8,
    },
    Literal {
        expected: &'static [u8],
        position: usize,
    },
    Number(Number),
}

#[derive(Debug, Clone, Copy)]
enum Number {
    Minus,
    Zero,
    Integer,
    Dot,
    Fraction,
    Exponent,
    Sign,
    ExponentDigit,
}
impl Number {
    fn step(self, byte: u8) -> Result<Option<Self>> {
        use Number::*;
        Ok(Some(match (self, byte) {
            (Minus, b'0') => Zero,
            (Minus, b'1'..=b'9') => Integer,
            (Integer, b'0'..=b'9') => Integer,
            (Zero | Integer, b'.') => Dot,
            (Zero | Integer | Fraction, b'e' | b'E') => Exponent,
            (Dot | Fraction, b'0'..=b'9') => Fraction,
            (Exponent, b'+' | b'-') => Sign,
            (Exponent | Sign | ExponentDigit, b'0'..=b'9') => ExponentDigit,
            (
                Zero | Integer | Fraction | ExponentDigit,
                b' ' | b'\t' | b'\r' | b'\n' | b',' | b']' | b'}',
            ) => return Ok(None),
            _ => return Err(ReadFailure::HistoryUnknown),
        }))
    }
}

#[derive(Debug, Clone, Default)]
struct Parser {
    stack: Vec<Frame>,
    token: Option<Token>,
    record: Record,
    complete: bool,
    metadata: usize,
}

impl Parser {
    fn empty(&self) -> bool {
        self.stack.is_empty() && self.token.is_none() && !self.complete
    }

    // Skip only plain ASCII inside an ignored string. Quotes, escapes, control
    // bytes and UTF-8 still pass through the validating state machine. No body
    // bytes are copied or retained, including across the 64 KiB read boundary.
    fn skip_ignored_ascii(&self, input: &[u8]) -> usize {
        if !matches!(
            self.token,
            Some(Token::String {
                keep: false,
                escape: false,
                unicode: 0,
                utf8: 0,
                ..
            })
        ) {
            return 0;
        }
        input
            .iter()
            .position(|byte| !matches!(byte, 0x20..=0x7f) || matches!(byte, b'"' | b'\\'))
            .unwrap_or(input.len())
    }

    fn feed(&mut self, byte: u8) -> Result<Option<Record>> {
        if let Some(token) = self.token.take() {
            match token {
                Token::Literal { expected, position } => {
                    if expected.get(position) != Some(&byte) {
                        return Err(ReadFailure::HistoryUnknown);
                    }
                    if position + 1 == expected.len() {
                        self.value_done()?;
                    } else {
                        self.token = Some(Token::Literal {
                            expected,
                            position: position + 1,
                        });
                    }
                    return Ok(None);
                }
                Token::Number(number) => {
                    if let Some(number) = number.step(byte)? {
                        self.token = Some(Token::Number(number));
                        return Ok(None);
                    }
                    self.value_done()?;
                }
                Token::String {
                    target,
                    mut bytes,
                    mut keep,
                    mut escape,
                    mut unicode,
                    mut utf8,
                    mut low,
                    mut high,
                } => {
                    let finished = byte == b'"' && !escape && unicode == 0 && utf8 == 0;
                    if !finished {
                        if utf8 != 0 {
                            if byte < low || byte > high {
                                return Err(ReadFailure::HistoryUnknown);
                            }
                            utf8 -= 1;
                            low = 0x80;
                            high = 0xbf;
                        } else if unicode != 0 {
                            if !byte.is_ascii_hexdigit() {
                                return Err(ReadFailure::HistoryUnknown);
                            }
                            unicode -= 1;
                        } else if escape {
                            escape = false;
                            match byte {
                                b'u' => unicode = 4,
                                b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => {}
                                _ => return Err(ReadFailure::HistoryUnknown),
                            }
                        } else {
                            match byte {
                                0..=31 => return Err(ReadFailure::HistoryUnknown),
                                b'\\' => escape = true,
                                0xc2..=0xdf => utf8 = 1,
                                0xe0..=0xef => {
                                    utf8 = 2;
                                    if byte == 0xe0 {
                                        low = 0xa0;
                                    }
                                    if byte == 0xed {
                                        high = 0x9f;
                                    }
                                }
                                0xf0..=0xf4 => {
                                    utf8 = 3;
                                    if byte == 0xf0 {
                                        low = 0x90;
                                    }
                                    if byte == 0xf4 {
                                        high = 0x8f;
                                    }
                                }
                                0x80..=0xff => return Err(ReadFailure::HistoryUnknown),
                                _ => {}
                            }
                        }
                        if keep {
                            if bytes.len() >= crate::pane_state::IDENTIFIER_MAX_BYTES * 6 + 2 {
                                if matches!(target, Target::Key) {
                                    keep = false;
                                    bytes.clear();
                                } else {
                                    return Err(ReadFailure::HistoryUnknown);
                                }
                            } else {
                                bytes.push(byte);
                            }
                        }
                        self.token = Some(Token::String {
                            target,
                            bytes,
                            keep,
                            escape,
                            unicode,
                            utf8,
                            low,
                            high,
                        });
                        return Ok(None);
                    }
                    let value: Option<String> = if keep {
                        bytes.push(b'"');
                        Some(
                            serde_json::from_slice(&bytes)
                                .map_err(|_| ReadFailure::HistoryUnknown)?,
                        )
                    } else {
                        None
                    };
                    match target {
                        Target::Key => {
                            let frame = self.stack.last_mut().ok_or(ReadFailure::HistoryUnknown)?;
                            let field = if frame.role == Role::Other {
                                Field::Other
                            } else {
                                value.as_deref().map(Field::parse).unwrap_or_default()
                            };
                            if frame.seen & field.bit() != 0 {
                                return Err(ReadFailure::HistoryUnknown);
                            }
                            frame.seen |= field.bit();
                            frame.field = field;
                            frame.stage = Stage::Colon;
                        }
                        Target::Value(role, field) => {
                            if let Some(value) = value {
                                self.string_value(role, field, &value)?;
                            }
                            self.value_done()?;
                        }
                    }
                    return Ok(None);
                }
            }
        }
        if self.complete {
            if byte == b'\n' {
                self.complete = false;
                self.metadata = 0;
                return Ok(Some(std::mem::take(&mut self.record)));
            }
            if matches!(byte, b' ' | b'\t' | b'\r') {
                return Ok(None);
            }
            return Err(ReadFailure::HistoryUnknown);
        }
        if byte.is_ascii_whitespace() {
            if !matches!(byte, b' ' | b'\t' | b'\r' | b'\n') {
                return Err(ReadFailure::HistoryUnknown);
            }
            return Ok(None);
        }
        let Some(frame) = self.stack.last().cloned() else {
            if byte != b'{' {
                return Err(ReadFailure::HistoryUnknown);
            }
            self.push(Role::Root, false)?;
            return Ok(None);
        };
        match frame.stage {
            Stage::KeyOrEnd if byte == b'}' => self.close(false)?,
            Stage::KeyOrEnd | Stage::Key => {
                if byte != b'"' {
                    return Err(ReadFailure::HistoryUnknown);
                }
                self.begin_string(Target::Key, frame.role != Role::Other);
            }
            Stage::Colon => {
                if byte != b':' {
                    return Err(ReadFailure::HistoryUnknown);
                }
                self.stack.last_mut().expect("frame").stage = Stage::Value;
            }
            Stage::ValueOrEnd if byte == b']' => self.close(true)?,
            Stage::Value | Stage::ValueOrEnd => self.begin_value(&frame, byte)?,
            Stage::CommaObject if byte == b'}' => self.close(false)?,
            Stage::CommaArray if byte == b']' => self.close(true)?,
            Stage::CommaObject | Stage::CommaArray if byte == b',' => {
                self.stack.last_mut().expect("frame").stage = if frame.array {
                    Stage::Value
                } else {
                    Stage::Key
                };
            }
            _ => return Err(ReadFailure::HistoryUnknown),
        }
        Ok(None)
    }

    fn begin_string(&mut self, target: Target, keep: bool) {
        self.token = Some(Token::String {
            target,
            bytes: if keep { vec![b'"'] } else { Vec::new() },
            keep,
            escape: false,
            unicode: 0,
            utf8: 0,
            low: 0x80,
            high: 0xbf,
        });
    }

    fn begin_value(&mut self, frame: &Frame, byte: u8) -> Result<()> {
        let field = if frame.array {
            Field::Other
        } else {
            frame.field
        };
        if frame.role == Role::Payload {
            if field == Field::Error {
                self.record.error = byte != b'n';
            }
            if field == Field::Parent {
                self.record.parent = byte != b'n';
            }
        }
        match byte {
            b'{' | b'[' => {
                let role = if frame.role == Role::Root && field == Field::Payload && byte == b'{' {
                    Role::Payload
                } else {
                    Role::Other
                };
                self.push(role, byte == b'[')?;
            }
            b'"' => self.begin_string(
                Target::Value(frame.role, field),
                matches!(
                    (frame.role, field),
                    (Role::Root, Field::Type)
                        | (
                            Role::Payload,
                            Field::Type | Field::Id | Field::Turn | Field::Source | Field::Parent
                        )
                ),
            ),
            b'n' => {
                self.token = Some(Token::Literal {
                    expected: b"null",
                    position: 1,
                })
            }
            b't' => {
                self.token = Some(Token::Literal {
                    expected: b"true",
                    position: 1,
                })
            }
            b'f' => {
                self.token = Some(Token::Literal {
                    expected: b"false",
                    position: 1,
                })
            }
            b'-' => self.token = Some(Token::Number(Number::Minus)),
            b'0' => self.token = Some(Token::Number(Number::Zero)),
            b'1'..=b'9' => self.token = Some(Token::Number(Number::Integer)),
            _ => return Err(ReadFailure::HistoryUnknown),
        }
        Ok(())
    }

    fn string_value(&mut self, role: Role, field: Field, value: &str) -> Result<()> {
        self.metadata += value.len();
        if value.len() > crate::pane_state::IDENTIFIER_MAX_BYTES || self.metadata > MAX_METADATA {
            return Err(ReadFailure::HistoryUnknown);
        }
        match (role, field) {
            (Role::Root, Field::Type) => self.record.kind = Kind::parse(value),
            (Role::Payload, Field::Type) => self.record.event = Kind::parse(value),
            (Role::Payload, Field::Id) => self.record.id = Some(identifier_digest(value)),
            (Role::Payload, Field::Turn) => self.record.turn = Some(identifier_digest(value)),
            (Role::Payload, Field::Source) => self.record.user_source = value == "user",
            (Role::Payload, Field::Parent) => self.record.parent = !value.is_empty(),
            _ => {}
        }
        Ok(())
    }

    fn push(&mut self, role: Role, array: bool) -> Result<()> {
        if self.stack.len() >= MAX_DEPTH {
            return Err(ReadFailure::HistoryUnknown);
        }
        self.stack.push(Frame {
            role,
            stage: if array {
                Stage::ValueOrEnd
            } else {
                Stage::KeyOrEnd
            },
            field: Field::Other,
            seen: 0,
            array,
        });
        Ok(())
    }
    fn close(&mut self, array: bool) -> Result<()> {
        if self.stack.pop().is_none_or(|frame| frame.array != array) {
            return Err(ReadFailure::HistoryUnknown);
        }
        if self.stack.is_empty() {
            self.complete = true;
        } else {
            self.value_done()?;
        }
        Ok(())
    }
    fn value_done(&mut self) -> Result<()> {
        let frame = self.stack.last_mut().ok_or(ReadFailure::HistoryUnknown)?;
        if !matches!(frame.stage, Stage::Value | Stage::ValueOrEnd) {
            return Err(ReadFailure::HistoryUnknown);
        }
        frame.stage = if frame.array {
            Stage::CommaArray
        } else {
            Stage::CommaObject
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::Duration;

    struct Fixture {
        root: std::path::PathBuf,
        path: std::path::PathBuf,
        cursor: Cursor,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "vt-turn-order-{}",
                crate::pane_state::EventId::generate().unwrap().as_str()
            ));
            std::fs::create_dir_all(root.join("sessions")).unwrap();
            let root = root.canonicalize().unwrap();
            let path = root.join("sessions/synthetic.jsonl");
            std::fs::write(&path, b"{\"payload\":{\"thread_source\":\"user\",\"id\":\"synthetic\"},\"type\":\"session_meta\"}\n").unwrap();
            let locator = TranscriptLocator::capture(&root, &path).unwrap();
            let cursor = Cursor::new(locator, identifier_digest("synthetic"));
            Self { root, path, cursor }
        }
        fn append(&self, text: &str) {
            std::fs::OpenOptions::new()
                .append(true)
                .open(&self.path)
                .unwrap()
                .write_all(text.as_bytes())
                .unwrap();
        }
        fn read(&mut self) -> Result<SliceResult> {
            self.cursor
                .read_slice(SLICE_BYTES, Instant::now() + Duration::from_secs(10))
        }
        fn event(&self, kind: &str, turn: &str, extra: &str) {
            self.append(&format!("{{\"type\":\"event_msg\",\"payload\":{{\"turn_id\":\"{turn}\",\"type\":\"{kind}\"{extra}}}}}\n"));
        }
        fn proves(&self, a: &str, b: &str) -> bool {
            self.cursor
                .proves(&identifier_digest(a), &identifier_digest(b))
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn ignored_body_fast_path_preserves_json_validation_across_buffers() {
        for (tail, valid) in [
            ("日本語\\n\\u0061\\\"".as_bytes().to_vec(), true),
            (vec![1], false),
            (vec![0xff], false),
            (b"\\x".to_vec(), false),
            (vec![0xe0, 0x80, 0x80], false),
        ] {
            let mut f = Fixture::new();
            let mut body = b"{\"type\":\"response_item\",\"payload\":{\"text\":\"".to_vec();
            body.extend(std::iter::repeat_n(b'a', 70_000));
            body.extend(tail);
            body.extend_from_slice(b"\"}}\n");
            std::fs::OpenOptions::new()
                .append(true)
                .open(&f.path)
                .unwrap()
                .write_all(&body)
                .unwrap();
            f.event("task_started", "a", "");
            f.event("task_complete", "a", "");
            f.event("task_started", "b", "");
            if valid {
                assert!(f.read().is_ok());
                assert!(f.proves("a", "b"));
            } else {
                assert_eq!(f.read(), Err(ReadFailure::HistoryUnknown));
                assert!(!f.proves("a", "b"));
            }
        }
    }

    #[test]
    fn native_and_alias_events_prove_each_issued_turn_in_both_history_modes() {
        for (start, complete) in [
            ("task_started", "task_complete"),
            ("turn_started", "turn_complete"),
        ] {
            for history in ["legacy", "paginated"] {
                let mut f = Fixture::new();
                std::fs::write(&f.path, format!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"synthetic\",\"thread_source\":\"user\",\"history_mode\":\"{history}\"}}}}\n"
                )).unwrap();
                f.event(start, "a1", "");
                f.event(complete, "a1", ",\"error\":null");
                f.event(start, "a2", "");
                f.event(complete, "a2", "");
                f.event(start, "b", "");
                assert!(!f.read().unwrap().more);
                assert!(f.proves("a1", "b") && f.proves("a2", "b"));
                assert!(!f.proves("b", "b") && !f.proves("b", "a1"));
            }
        }
    }

    #[test]
    fn reverse_delivery_error_abort_and_missing_order_never_prove_resolution() {
        let mut f = Fixture::new();
        f.event("task_started", "b", "");
        f.event("task_complete", "b", "");
        f.event("task_started", "c", "");
        f.event("task_complete", "c", "");
        f.event("task_started", "error", "");
        f.event(
            "task_complete",
            "error",
            ",\"error\":{\"body\":\"synthetic-error-canary\"}",
        );
        f.event("task_started", "aborted", "");
        f.event("turn_aborted", "aborted", "");
        f.event("task_started", "latest", "");
        f.read().unwrap();
        for (a, b) in [
            ("c", "b"),
            ("error", "latest"),
            ("aborted", "latest"),
            ("missing", "latest"),
        ] {
            assert!(!f.proves(a, b));
        }
        assert!(!format!("{:?}", f.cursor).contains("synthetic-error-canary"));
    }

    #[test]
    fn giant_body_partial_record_and_budget_exhaustion_preserve_the_cursor() {
        let mut f = Fixture::new();
        f.event("task_started", "a", "");
        f.append("{\"payload\":{\"last_agent_message\":\"");
        let block = "BODY_CANARY_".repeat(8192);
        for _ in 0..360 {
            f.append(&block);
        }
        let mut total = 0;
        while total < REQUEST_BYTES {
            let result = f.read().unwrap();
            total += result.bytes;
            assert!(result.partial);
            assert!(!format!("{:?}", f.cursor).contains("BODY_CANARY"));
        }
        assert!(!f.proves("a", "b"));
        // A later, different ingress may resume from the same finite lexer state.
        loop {
            if !f.read().unwrap().more {
                break;
            }
        }
        let at = f.cursor.offset;
        assert!(f.read().unwrap().partial);
        assert_eq!(f.cursor.offset, at);
        f.append("\",\"error\":null,\"turn_id\":\"a\",\"type\":\"task_complete\"},\"type\":\"event_msg\"}\n");
        f.event("task_started", "b", "");
        f.read().unwrap();
        assert!(f.proves("a", "b"));
    }

    #[test]
    fn malformed_json_duplicate_rollback_and_file_replacement_invalidate_history() {
        for record in [
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"thread_rolled_back\"}}\n",
            "{\"type\":\"event_msg\",\"type\":\"event_msg\"}\n",
            "{\"body\": [1,]}\n",
            "{\"body\": \"bad\\q\"}\n",
            "{\"body\": 01}\n",
        ] {
            let mut f = Fixture::new();
            f.append(record);
            assert_eq!(f.read(), Err(ReadFailure::HistoryUnknown));
            assert!(!f.cursor.header_verified());
        }
        let mut f = Fixture::new();
        f.event("task_started", "a", "");
        f.event("task_started", "a", "");
        assert_eq!(f.read(), Err(ReadFailure::HistoryUnknown));
        let mut f = Fixture::new();
        f.read().unwrap();
        std::fs::write(&f.path, b"").unwrap();
        assert_eq!(f.read(), Err(ReadFailure::HistoryUnknown));
        let mut f = Fixture::new();
        f.read().unwrap();
        let replacement = f.root.join("replacement");
        std::fs::write(&replacement, b"{}\n").unwrap();
        std::fs::rename(replacement, &f.path).unwrap();
        assert_eq!(f.read(), Err(ReadFailure::HistoryUnknown));
    }

    #[test]
    fn streaming_grammar_matches_serde_on_synthetic_nested_values_at_every_split() {
        for json in [
            r#"{"body":{"value":[null,true,false,-3.2e+5,{"unicode":"\u65e5本🙂"}]},"type":"other"}"#,
            r#"{"payload":null,"body":"escaped\"\\\/\n","type":"other"}"#,
        ] {
            serde_json::from_str::<serde_json::Value>(json).unwrap();
            for split in 0..json.len() {
                let mut parser = Parser::default();
                for part in [&json.as_bytes()[..split], &json.as_bytes()[split..], b"\n"] {
                    for byte in part {
                        parser.feed(*byte).unwrap();
                    }
                }
                assert!(parser.empty());
            }
        }
    }
}
