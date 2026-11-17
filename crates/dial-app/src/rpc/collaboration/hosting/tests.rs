use super::*;

fn code(error: anyhow::Error) -> ErrorCode {
    error
        .downcast_ref::<super::super::super::RequestError>()
        .unwrap()
        .code
}

#[test]
fn sharing_requires_explicit_consent_before_network_or_archive_access() {
    for params in [
        json!({}),
        json!({"confirm":false}),
        json!({"confirm":"true"}),
        json!({"confirm":true,"read_only":"false"}),
        json!({"confirm":true,"unknown":true}),
    ] {
        assert_eq!(
            code(request(&params).err().unwrap()),
            ErrorCode::InvalidParams
        );
    }
    assert!(request(&json!({"confirm":true})).unwrap().read_only);
    assert!(
        !request(&json!({"confirm":true,"read_only":false}))
            .unwrap()
            .read_only
    );
    for params in [
        json!({}),
        json!({"confirm":false}),
        json!({"confirm":true,"unknown":true}),
    ] {
        assert_eq!(code(invite(&params).unwrap_err()), ErrorCode::InvalidParams);
    }
}

#[test]
fn host_is_connection_owned_session_bound_and_invites_are_explicit() {
    use std::{net::TcpListener, time::Duration};
    let _lock = super::super::TEST_ROOM.lock().unwrap();
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            disconnect();
        }
    }
    let _cleanup = Cleanup;
    let temp = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(temp.path(), temp.path().join("user")).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut config = Config::default();
    config.collab.relay_url = format!("ws://{}", listener.local_addr().unwrap());
    // An actual local WebSocket endpoint, with no external service. Owner admission/authentication
    // is already exercised by collab::server::tests with the full encrypted relay protocol.
    let (ready, connected) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        for _ in 0..2 {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            ready.send(()).unwrap();
            while socket.read().is_ok() {}
        }
    });
    let session = Session::ephemeral();
    let cancelled = AtomicBool::new(true);
    assert_eq!(
        code(
            start(
                &config,
                &layout,
                &session,
                &json!({"confirm":true}),
                &cancelled
            )
            .unwrap_err()
        ),
        ErrorCode::Conflict
    );
    let cancel = AtomicBool::new(false);
    for read_only in [true, false] {
        let result = start(
            &config,
            &layout,
            &session,
            &json!({"confirm":true,"read_only":read_only}),
            &cancel,
        )
        .unwrap();
        assert!(result.get("link").is_none());
        let status = room::status();
        connected.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(status["role"], "host");
        assert_eq!(status["read_only"], read_only);
        let invite = invite(&json!({"confirm":true})).unwrap();
        let parsed =
            crate::collab::protocol::parse_link(invite["link"].as_str().unwrap(), "").unwrap();
        assert_eq!(parsed.write_token.is_none(), read_only);
        assert!(
            !status
                .to_string()
                .contains(invite["link"].as_str().unwrap())
        );
        assert_eq!(
            code(
                start(
                    &config,
                    &layout,
                    &session,
                    &json!({"confirm":true}),
                    &cancel
                )
                .unwrap_err()
            ),
            ErrorCode::Busy
        );
        sync(&session);
        assert_eq!(room::status()["role"], "host");
        if read_only {
            sync(&Session::ephemeral());
        } else {
            disconnect();
        }
        assert_eq!(room::status()["role"], "none");
        assert_eq!(code(stop().unwrap_err()), ErrorCode::NotPending);
        assert!(
            crate::collab::registry::list_live(&layout.user_dir)
                .unwrap()
                .is_empty()
        );
    }
    worker.join().unwrap();
    // Teardown cannot claim a guest that belongs to another lifecycle.
    room::with(|slot| *slot = Some(room::Room::Guest(crate::collab::guest::Guest::fixture(&[]))));
    disconnect();
    assert!(room::is_guest());
    room::leave().unwrap();
}
