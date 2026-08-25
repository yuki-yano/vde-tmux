use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};

use super::{
    V2_FRAME_BODY_TIMEOUT, V2_FRAME_START_TIMEOUT, V2_OVERLOAD_RESPONSE_WRITE_TIMEOUT,
    V2_RESPONSE_WRITE_TIMEOUT,
};

#[cfg(test)]
mod tests;

#[derive(Default)]
pub(super) struct V2ConnectionThreadCounts {
    pub(super) active: usize,
    pub(super) streaming: usize,
}

pub(super) struct V2ConnectionThreadLimiter {
    pub(super) capacity: usize,
    pub(super) streaming_capacity: usize,
    pub(super) counts: Mutex<V2ConnectionThreadCounts>,
}

impl V2ConnectionThreadLimiter {
    pub(super) fn new(capacity: usize, reserved_non_streaming: usize) -> Self {
        assert!(reserved_non_streaming <= capacity);
        Self {
            capacity,
            streaming_capacity: capacity - reserved_non_streaming,
            counts: Mutex::new(V2ConnectionThreadCounts::default()),
        }
    }

    pub(super) fn try_acquire(self: &Arc<Self>) -> Option<V2ConnectionThreadPermit> {
        let mut counts = self
            .counts
            .lock()
            .expect("v2 connection thread limiter lock poisoned");
        if counts.active >= self.capacity {
            return None;
        }
        counts.active += 1;
        Some(V2ConnectionThreadPermit {
            limiter: self.clone(),
            streaming: false,
        })
    }
}

pub(super) struct V2ConnectionThreadPermit {
    pub(super) limiter: Arc<V2ConnectionThreadLimiter>,
    pub(super) streaming: bool,
}

impl V2ConnectionThreadPermit {
    pub(super) fn try_mark_streaming(&mut self) -> bool {
        if self.streaming {
            return true;
        }
        let mut counts = self
            .limiter
            .counts
            .lock()
            .expect("v2 connection thread limiter lock poisoned");
        if counts.streaming >= self.limiter.streaming_capacity {
            return false;
        }
        counts.streaming += 1;
        self.streaming = true;
        true
    }
}

impl Drop for V2ConnectionThreadPermit {
    fn drop(&mut self) {
        let mut counts = self
            .limiter
            .counts
            .lock()
            .expect("v2 connection thread limiter lock poisoned");
        counts.active = counts
            .active
            .checked_sub(1)
            .expect("v2 connection thread permit released once");
        if self.streaming {
            counts.streaming = counts
                .streaming
                .checked_sub(1)
                .expect("v2 streaming connection permit released once");
        }
    }
}

pub struct V2FrameReader {
    reader: BufReader<UnixStream>,
}

impl V2FrameReader {
    pub fn new(stream: UnixStream) -> Self {
        Self {
            reader: BufReader::new(stream),
        }
    }

    pub fn stream_mut(&mut self) -> &mut UnixStream {
        self.reader.get_mut()
    }

    pub fn into_stream(self) -> UnixStream {
        self.reader.into_inner()
    }
}

#[allow(clippy::result_large_err)]
pub fn read_v2_request_frame(
    connection: &mut V2FrameReader,
) -> std::result::Result<Vec<u8>, ServerMessage> {
    use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};
    use crate::pane_state::MAX_REQUEST_FRAME_BYTES;

    connection
        .reader
        .get_mut()
        .set_read_timeout(Some(V2_FRAME_START_TIMEOUT))
        .map_err(|error| ServerMessage::error(ErrorCode::InternalError, error.to_string(), None))?;
    let mut frame = Vec::new();
    let mut body_deadline: Option<std::time::Instant> = None;
    loop {
        if let Some(deadline) = body_deadline {
            let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
                return Err(ServerMessage::error(
                    ErrorCode::InvalidRequest,
                    "request frame body deadline exceeded",
                    None,
                ));
            };
            connection
                .reader
                .get_mut()
                .set_read_timeout(Some(remaining))
                .map_err(|error| {
                    ServerMessage::error(ErrorCode::InternalError, error.to_string(), None)
                })?;
        }
        let available = connection.reader.fill_buf().map_err(|error| {
            let stage = if body_deadline.is_some() {
                "body"
            } else {
                "start"
            };
            ServerMessage::error(
                ErrorCode::InvalidRequest,
                format!("request frame {stage} deadline exceeded: {error}"),
                None,
            )
        })?;
        if available.is_empty() {
            return Err(ServerMessage::error(
                ErrorCode::InvalidRequest,
                "connection closed before request frame completed",
                None,
            ));
        }
        if body_deadline.is_none() {
            body_deadline = Some(std::time::Instant::now() + V2_FRAME_BODY_TIMEOUT);
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |position| position + 1);
        let body_bytes = request_frame_body_bytes(frame.len(), take, newline.is_some());
        if body_bytes > MAX_REQUEST_FRAME_BYTES {
            return Err(ServerMessage::error(
                ErrorCode::FrameTooLarge,
                "request frame exceeds 1 MiB",
                None,
            ));
        }
        frame.extend_from_slice(&available[..take]);
        connection.reader.consume(take);
        if newline.is_some() {
            frame.pop();
            return Ok(frame);
        }
    }
}

pub(super) fn request_frame_body_bytes(
    buffered: usize,
    take: usize,
    newline_terminated: bool,
) -> usize {
    buffered
        .saturating_add(take)
        .saturating_sub(usize::from(newline_terminated))
}

#[allow(clippy::result_large_err)]
pub fn write_v2_response(
    stream: &mut UnixStream,
    message: &ServerMessage,
) -> std::result::Result<(), ServerMessage> {
    use crate::daemon::protocol::v2::{ErrorCode, ServerMessage, encode_response_frame};

    let frame = match encode_response_frame(message) {
        Ok(frame) => frame,
        Err(
            error @ ServerMessage::Error {
                code: ErrorCode::FrameTooLarge,
                ..
            },
        ) => encode_response_frame(&error)?,
        Err(error) => return Err(error),
    };
    write_v2_frame(stream, &frame)
}

#[allow(clippy::result_large_err)]
pub(super) fn write_v2_frame(
    stream: &mut UnixStream,
    frame: &[u8],
) -> std::result::Result<(), ServerMessage> {
    write_v2_frame_with_timeout(stream, frame, V2_RESPONSE_WRITE_TIMEOUT)
}

#[allow(clippy::result_large_err)]
pub(super) fn write_v2_frame_with_timeout(
    stream: &mut UnixStream,
    frame: &[u8],
    timeout: Duration,
) -> std::result::Result<(), ServerMessage> {
    use crate::daemon::protocol::v2::{ErrorCode, ServerMessage};

    let deadline = std::time::Instant::now() + timeout;
    let mut written = 0;
    while written < frame.len() {
        let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
            return Err(ServerMessage::error(
                ErrorCode::InternalError,
                "response write deadline exceeded",
                None,
            ));
        };
        let timeout = bounded_write_timeout(remaining);
        stream.set_write_timeout(Some(timeout)).map_err(|error| {
            ServerMessage::error(ErrorCode::InternalError, error.to_string(), None)
        })?;
        let count = stream.write(&frame[written..]).map_err(|error| {
            ServerMessage::error(
                ErrorCode::InternalError,
                format!("response write failed: {error}"),
                None,
            )
        })?;
        if count == 0 {
            return Err(ServerMessage::error(
                ErrorCode::InternalError,
                "response stream closed before frame completed",
                None,
            ));
        }
        written += count;
    }
    Ok(())
}

pub(super) fn write_v2_overload_response(stream: &mut UnixStream) {
    let response = ServerMessage::error(
        ErrorCode::QueueFull,
        "daemon connection capacity is full",
        None,
    );
    if let Ok(frame) = crate::daemon::protocol::v2::encode_response_frame(&response) {
        let _ = write_v2_frame_with_timeout(stream, &frame, V2_OVERLOAD_RESPONSE_WRITE_TIMEOUT);
    }
}

pub(super) fn bounded_write_timeout(remaining: Duration) -> Duration {
    remaining.max(Duration::from_millis(1))
}
