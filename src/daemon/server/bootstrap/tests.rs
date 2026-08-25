use super::super::*;
use super::*;

#[test]
fn runtime_cleanup_removes_owned_socket_and_process_record_on_early_return() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let event_id = EventId::generate().unwrap();
    let root = PathBuf::from(format!(
        "/tmp/vrc-{}-{}",
        std::process::id(),
        &event_id.as_str()[..8]
    ));
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = root.join("daemon.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let metadata = std::fs::symlink_metadata(&socket).unwrap();
    let identity = crate::daemon::lifecycle::DaemonProcessIdentity {
        pid: std::process::id(),
        start_token: crate::daemon::lifecycle::process_start_token(std::process::id()).unwrap(),
        daemon_instance_id: V2_DAEMON_ID.to_string(),
        socket_device: metadata.dev(),
        socket_inode: metadata.ino(),
    };
    let env = BTreeMap::from([(
        "XDG_STATE_HOME".to_string(),
        root.to_string_lossy().into_owned(),
    )]);
    let incarnation_hash = "runtime-cleanup-test";
    crate::daemon::lifecycle::update_lifecycle_record(&env, incarnation_hash, |record| {
        record.process = Some(identity.clone());
        Ok(())
    })
    .unwrap();

    drop(RuntimeDaemonCleanup::new(
        &env,
        incarnation_hash,
        &socket,
        identity,
    ));

    assert!(!socket.exists());
    assert!(
        crate::daemon::lifecycle::read_lifecycle_record(&env, incarnation_hash)
            .unwrap()
            .process
            .is_none()
    );
    drop(listener);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn post_bind_initialization_failure_removes_socket_and_releases_instance_lock() {
    use std::os::unix::fs::PermissionsExt as _;

    let event_id = EventId::generate().unwrap();
    let root = PathBuf::from(format!(
        "/tmp/vpb-{}-{}",
        std::process::id(),
        &event_id.as_str()[..8]
    ));
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = root.join("daemon.sock");
    let env = BTreeMap::from([(
        "XDG_STATE_HOME".to_string(),
        root.to_string_lossy().into_owned(),
    )]);
    let incarnation_hash = "c".repeat(64);
    crate::daemon::lifecycle::update_lifecycle_record(&env, &incarnation_hash, |_| Ok(())).unwrap();
    let lifecycle_path = crate::daemon::lifecycle::lifecycle_record_path(&env, &incarnation_hash);
    let malformed_record = b"{malformed lifecycle record\n";
    std::fs::write(&lifecycle_path, malformed_record).unwrap();

    let Some((listener, instance_lock, socket_cleanup)) = bind_daemon_listener(&socket).unwrap()
    else {
        panic!("test must acquire the daemon instance lock");
    };
    assert!(
        crate::daemon::lifecycle::try_acquire_daemon_instance_lock(&socket)
            .unwrap()
            .is_none()
    );

    let result = initialize_runtime_daemon_post_bind(
        &crate::config::Config::default(),
        &socket,
        &env,
        crate::daemon::lifecycle::TmuxServerIncarnation {
            socket_path: root.join("tmux.sock"),
            identity: crate::daemon::topology::ServerIdentity {
                pid: 1,
                start_time: 2,
            },
            hash: incarnation_hash,
        },
        socket_cleanup,
    );
    let error = match result {
        Ok(_) => panic!("malformed lifecycle record must fail post-bind initialization"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("invalid lifecycle record"));
    assert!(!socket.exists());
    assert_eq!(std::fs::read(&lifecycle_path).unwrap(), malformed_record);

    drop(listener);
    drop(instance_lock);
    let reacquired = crate::daemon::lifecycle::try_acquire_daemon_instance_lock(&socket)
        .unwrap()
        .expect("instance lock must be released after the early return");
    drop(reacquired);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn bound_socket_cleanup_preserves_replacement_socket() {
    let event_id = EventId::generate().unwrap();
    let root = PathBuf::from(format!(
        "/tmp/vbs-{}-{}",
        std::process::id(),
        &event_id.as_str()[..8]
    ));
    std::fs::create_dir(&root).unwrap();
    let socket = root.join("daemon.sock");
    let original_listener = UnixListener::bind(&socket).unwrap();
    let cleanup = BoundDaemonSocketCleanup::new(&socket).unwrap();
    std::fs::remove_file(&socket).unwrap();
    let replacement_listener = UnixListener::bind(&socket).unwrap();
    let replacement_identity = crate::daemon::lifecycle::daemon_process_identity(
        &socket,
        &DaemonInstanceId::parse(V2_DAEMON_ID.to_string()).unwrap(),
    )
    .unwrap();

    assert!(
        cleanup
            .verify_process_identity(&replacement_identity)
            .is_err()
    );

    drop(cleanup);

    assert!(socket.exists());
    drop(replacement_listener);
    std::fs::remove_file(&socket).unwrap();
    drop(original_listener);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn shutdown_forwarder_stops_v2_coordinator() {
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    let (mut signal_writer, signal_reader) = UnixStream::pair().unwrap();
    let coordinator = Arc::new(detached_test_coordinator("1".repeat(64)));
    spawn_shutdown_forwarder(signal_reader, coordinator.clone());
    let (done_tx, done_rx) = mpsc::channel();
    let waiter = {
        let coordinator = coordinator.clone();
        thread::spawn(move || {
            coordinator.wait_for_shutdown();
            done_tx.send(()).unwrap();
        })
    };

    signal_writer.write_all(b"x").unwrap();

    done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    waiter.join().unwrap();
    assert!(coordinator.shutdown.load(Ordering::SeqCst));
    assert!(coordinator.shutdown_ready.load(Ordering::SeqCst));
    assert!(coordinator.router.lock().unwrap().is_fatal());
}
