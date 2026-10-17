use super::*;

#[test]
fn tcp_adapter_fixture_exchanges_dap_frames() {
    if let Some(port) = std::env::var_os("DIAL_DAP_FIXTURE_PORT") {
        let listener =
            TcpListener::bind(("127.0.0.1", port.to_string_lossy().parse::<u16>().unwrap()))
                .unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(&mut stream);
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.strip_prefix("Content-Length:") {
                content_length = value.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; content_length];
        reader.read_exact(&mut body).unwrap();
        let request: Value = serde_json::from_slice(&body).unwrap();
        drop(reader);
        assert_eq!(request["command"], "initialize");
        let response = json!({
            "seq":2,"type":"response","request_seq":request["seq"],
            "command":"initialize","success":true,"body":{"connected":true}
        })
        .to_string();
        write!(
            stream,
            "Content-Length: {}\r\n\r\n{}",
            response.len(),
            response
        )
        .unwrap();
        stream.flush().unwrap();
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let binary = std::env::current_exe().unwrap();
    let config = DapAdapterConfig {
        program: binary,
        args: vec![
            "tcp_adapter_fixture_exchanges_dap_frames".into(),
            "--nocapture".into(),
        ],
        env: BTreeMap::from([("DIAL_DAP_FIXTURE_PORT".into(), "${port}".into())]),
        timeout_secs: 5,
        transport: DapTransport::Tcp,
        address: String::new(),
        file_types: Vec::new(),
        root_markers: Vec::new(),
        launch_defaults: serde_json::Map::new(),
        attach_defaults: serde_json::Map::new(),
    };
    let active = Mutex::new(None);
    let cancellation = AtomicU64::new(0);
    let cancel = AtomicBool::new(false);
    let interrupted = AtomicBool::new(false);
    let operation = Operation {
        end: Cell::new(Some(Instant::now() + Duration::from_secs(5))),
        generation: 0,
        cancellation: &cancellation,
        cancel: &cancel,
        interrupted: &interrupted,
    };
    let session = Session::start(&config, dir.path(), &active, &operation).unwrap();
    let response = session
        .request("initialize", json!({}), &operation)
        .unwrap();
    assert_eq!(response["connected"], true);
}
