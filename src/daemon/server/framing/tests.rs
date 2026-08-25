use super::super::*;
use super::*;

#[test]
fn connection_thread_limiter_enforces_cap_and_releases_slots() {
    let limiter = Arc::new(V2ConnectionThreadLimiter::new(2, 1));
    let first = limiter.try_acquire().expect("first connection fits");
    let second = limiter.try_acquire().expect("second connection fits");
    assert!(limiter.try_acquire().is_none());

    drop(first);
    let replacement = limiter.try_acquire();
    assert!(replacement.is_some());

    drop(second);
    drop(replacement);
    assert_eq!(limiter.counts.lock().expect("limiter lock").active, 0);
}

#[test]
fn connection_overload_returns_queue_full_without_a_handler_thread() {
    let (mut server, client) = UnixStream::pair().unwrap();
    write_v2_overload_response(&mut server);

    let mut response = String::new();
    BufReader::new(client).read_line(&mut response).unwrap();
    assert!(matches!(
        serde_json::from_str::<ServerMessage>(&response).unwrap(),
        ServerMessage::Error {
            code: ErrorCode::QueueFull,
            ..
        }
    ));
}

#[test]
fn connection_thread_permit_releases_during_unwind() {
    let limiter = Arc::new(V2ConnectionThreadLimiter::new(1, 0));
    let permit = limiter.try_acquire().expect("connection fits");

    let result = std::panic::catch_unwind(move || {
        let _permit = permit;
        panic!("simulated connection handler panic");
    });

    assert!(result.is_err());
    assert!(limiter.try_acquire().is_some());
}

#[test]
fn streaming_connections_leave_reserved_non_streaming_capacity() {
    let limiter = Arc::new(V2ConnectionThreadLimiter::new(4, 1));
    let mut streaming = (0..3)
        .map(|_| limiter.try_acquire().expect("streaming connection fits"))
        .collect::<Vec<_>>();
    assert!(
        streaming
            .iter_mut()
            .all(|permit| permit.try_mark_streaming())
    );

    let mut reserved = limiter.try_acquire().expect("reserved connection fits");
    assert!(!reserved.try_mark_streaming());
    assert!(limiter.try_acquire().is_none());

    drop(streaming.pop());
    assert!(reserved.try_mark_streaming());
}
#[test]
fn v2_frame_body_deadline_is_typed_and_bounded() {
    let (server, mut client) = UnixStream::pair().unwrap();
    let mut reader = V2FrameReader::new(server);
    client.write_all(b"{").unwrap();
    let started = std::time::Instant::now();
    let error = read_v2_request_frame(&mut reader).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(matches!(
        error,
        ServerMessage::Error {
            code: ErrorCode::InvalidRequest,
            ..
        }
    ));
}

#[test]
fn v2_frame_reader_and_writer_use_newline_framing() {
    let (server, mut client) = UnixStream::pair().unwrap();
    let mut reader = V2FrameReader::new(server);
    write!(
        client,
        "{{\"op\":\"hello\",\"proto\":{PROTOCOL_VERSION}}}\n\
         {{\"op\":\"query_resolved_snapshot\",\"proto\":{PROTOCOL_VERSION}}}\n"
    )
    .unwrap();
    let frame = read_v2_request_frame(&mut reader).unwrap();
    assert_eq!(
        crate::daemon::protocol::v2::decode_request_frame(&frame).unwrap(),
        ClientMessage::Hello {
            proto: PROTOCOL_VERSION,
        }
    );
    let second = read_v2_request_frame(&mut reader).unwrap();
    assert_eq!(
        crate::daemon::protocol::v2::decode_request_frame(&second).unwrap(),
        ClientMessage::QueryResolvedSnapshot {
            proto: PROTOCOL_VERSION,
        }
    );
    let response = ServerMessage::error(ErrorCode::NotReady, "not ready", None);
    write_v2_response(reader.stream_mut(), &response).unwrap();
    let mut line = String::new();
    BufReader::new(client).read_line(&mut line).unwrap();
    assert_eq!(
        serde_json::from_str::<ServerMessage>(line.trim()).unwrap(),
        response
    );
}

#[test]
fn v2_request_frame_limit_counts_newline_only_when_present() {
    assert_eq!(
        request_frame_body_bytes(crate::pane_state::MAX_REQUEST_FRAME_BYTES, 1, true),
        crate::pane_state::MAX_REQUEST_FRAME_BYTES
    );
    assert_eq!(
        request_frame_body_bytes(crate::pane_state::MAX_REQUEST_FRAME_BYTES, 1, false),
        crate::pane_state::MAX_REQUEST_FRAME_BYTES + 1
    );
}

#[test]
fn v2_oversized_response_writes_typed_error_on_same_stream() {
    let (mut server, client) = UnixStream::pair().unwrap();
    let oversized = ServerMessage::error(
        ErrorCode::InternalError,
        "x".repeat(crate::pane_state::MAX_RESPONSE_FRAME_BYTES),
        None,
    );
    write_v2_response(&mut server, &oversized).unwrap();
    let mut line = String::new();
    BufReader::new(client).read_line(&mut line).unwrap();
    assert!(matches!(
        serde_json::from_str::<ServerMessage>(line.trim()).unwrap(),
        ServerMessage::Error {
            code: ErrorCode::FrameTooLarge,
            ..
        }
    ));
}

#[test]
fn response_write_timeout_has_one_millisecond_floor() {
    assert_eq!(
        bounded_write_timeout(Duration::from_nanos(1)),
        Duration::from_millis(1)
    );
    assert_eq!(
        bounded_write_timeout(Duration::from_millis(2)),
        Duration::from_millis(2)
    );
}
