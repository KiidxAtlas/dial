use super::*;

#[test]
fn server_requests_get_empty_but_well_formed_replies() {
    let root = "file:///work";
    let config = answer_server_request(
        "workspace/configuration",
        &json!({"items": [{}, {}, {}]}),
        root,
    );
    assert_eq!(config.unwrap(), json!([null, null, null]));
    let folders = answer_server_request("workspace/workspaceFolders", &Value::Null, root).unwrap();
    assert_eq!(folders[0]["uri"], root);
    for method in [
        "window/workDoneProgress/create",
        "client/registerCapability",
        "client/unregisterCapability",
        "workspace/diagnostic/refresh",
    ] {
        assert_eq!(
            answer_server_request(method, &Value::Null, root).unwrap(),
            Value::Null,
            "{method}"
        );
    }
    let apply = answer_server_request("workspace/applyEdit", &Value::Null, root).unwrap();
    assert_eq!(apply["applied"], false);
    assert_eq!(
        answer_server_request("custom/thing", &Value::Null, root)
            .unwrap_err()
            .0,
        -32601
    );
}

#[test]
fn configuration_is_answered_from_server_settings() {
    let settings = json!({"rust-analyzer": {"checkOnSave": false, "cargo": {"features": "all"}}});
    let answer = configuration(
        &settings,
        &json!({"items": [
            {"section": "rust-analyzer"}, {"section": "rust-analyzer.cargo"}, {"section": "missing"}, {}
        ]}),
    );
    assert_eq!(answer[0]["checkOnSave"], false);
    assert_eq!(answer[1]["features"], "all");
    assert_eq!(answer[2], Value::Null);
    assert_eq!(answer[3], settings);
}

#[test]
fn retry_only_content_modified_and_server_cancelled() {
    let modified: anyhow::Error = RpcError {
        code: CONTENT_MODIFIED,
        message: String::new(),
    }
    .into();
    let cancelled: anyhow::Error = RpcError {
        code: SERVER_CANCELLED,
        message: String::new(),
    }
    .into();
    let other: anyhow::Error = RpcError {
        code: -32603,
        message: String::new(),
    }
    .into();
    assert!(is_retryable(&modified) && is_retryable(&cancelled) && !is_retryable(&other));
}

#[test]
fn formatting_indentation_is_detected() {
    assert_eq!(indentation("fn a() {\n\tb();\n}\n"), (4, false));
    assert_eq!(indentation("a:\n  b: 1\n    c: 2\n"), (2, true));
    assert_eq!(indentation("x\n"), (4, true));
    assert_eq!(
        indentation("x\n    a\n    b\n        c\n        d\n  e\n"),
        (4, true)
    );
}

#[cfg(unix)]
#[test]
fn cancellation_interrupts_the_post_initialize_configuration_write() {
    use std::os::unix::net::UnixStream;

    struct BlockConfiguration {
        stream: UnixStream,
        blocked: Sender<()>,
        release: Receiver<()>,
    }
    impl Write for BlockConfiguration {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes
                .windows(b"workspace/didChangeConfiguration".len())
                .any(|window| window == b"workspace/didChangeConfiguration")
            {
                let _ = self.blocked.send(());
                let _ = self.release.recv();
            }
            self.stream.write(bytes)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.stream.flush()
        }
    }

    let (ours, theirs) = UnixStream::pair().unwrap();
    let mut reply = theirs.try_clone().unwrap();
    let server = thread::spawn(move || {
        let mut input = BufReader::new(theirs);
        let initialize = read_message(&mut input).unwrap().unwrap();
        write_message(
            &mut reply,
            &json!({"jsonrpc":"2.0", "id":initialize["id"], "result":{"capabilities":{}}}),
        )
        .unwrap();
        // The second notification is released only after cancellation returns.
        let _ = read_message(&mut input); // initialized
        let _ = read_message(&mut input); // didChangeConfiguration
    });
    let input = ours.try_clone().unwrap();
    let (blocked, observed) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let cancel = AtomicBool::new(false);
    thread::scope(|scope| {
        let worker = scope.spawn(|| {
            Client::connect_cancellable(
                "blocked",
                Path::new("/tmp"),
                Box::new(input),
                Box::new(BlockConfiguration {
                    stream: ours,
                    blocked,
                    release: released,
                }),
                None,
                Timeouts::default(),
                ServerOptions {
                    settings: json!({"test": true}),
                    ..Default::default()
                },
                &|| cancel.load(Ordering::Acquire),
            )
        });
        observed.recv_timeout(Duration::from_secs(2)).unwrap();
        let start = Instant::now();
        cancel.store(true, Ordering::Release);
        let result = worker.join().unwrap();
        let _ = release.send(());
        assert!(result.is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
    });
    server.join().unwrap();
}
