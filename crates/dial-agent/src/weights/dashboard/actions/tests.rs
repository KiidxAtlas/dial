use super::*;

fn config() -> (tempfile::TempDir, Config) {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        state_dir: dir.path().to_owned(),
        ..Config::default()
    };
    crate::weights::create_experiment(&config, "manual", 4).unwrap();
    (dir, config)
}

fn input(config: &Config) -> Value {
    json!({"model": crate::weights::target_name(config), "action": "apply",
        "experiment": "manual", "name": "scaled", "changes": [{"tensor": "model.layers.0.mlp.down_proj.weight", "scale": 0.5}]})
}

fn headers(body: &str) -> String {
    format!(
        "POST /api/scale HTTP/1.1\r\nHost: 127.0.0.1:4100\r\nOrigin: http://127.0.0.1:4100\r\nContent-Type: application/json\r\nX-Dial-Weights: 1\r\nContent-Length: {}",
        body.len()
    )
}

#[test]
fn scale_actions_use_existing_validators_and_force_scale_method() {
    let (_dir, config) = config();
    let mut input = input(&config);
    input["method"] = json!("steering");
    let body = input.to_string();
    let (op, value) = action_input(&config, &headers(&body), &body).unwrap();
    assert_eq!(op, Operation::Edit);
    assert_eq!(value["method"], "scale");
    assert_eq!(value["name"], "scaled");
    assert_eq!(value["changes"], input["changes"]);
    input["action"] = json!("preview");
    input["cases"] = json!([{"messages": [{"role": "user", "content": "Hi"}, {"role": "assistant", "content": "Hello"}]}]);
    let body = input.to_string();
    let (op, value) = action_input(&config, &headers(&body), &body).unwrap();
    assert_eq!(op, Operation::Try);
    assert_eq!(value["doses"], json!([1.0]));
    assert_eq!(value["cases"], input["cases"]);
}

#[test]
fn rejects_unsafe_invalid_and_stale_actions_before_spawning() {
    let (_dir, config) = config();
    let body = input(&config).to_string();
    let valid = headers(&body);
    for headers in [
        valid.replace("X-Dial-Weights: 1", "X-Dial-Weights: 0"),
        valid.replace(
            "Origin: http://127.0.0.1:4100",
            "Origin: http://evil.example",
        ),
        valid.replace("application/json", "text/plain"),
        format!("{valid}\r\nContent-Length: {}", body.len()),
        format!("{valid}\r\nTransfer-Encoding: chunked"),
        valid.replace(
            &format!("Content-Length: {}", body.len()),
            "Content-Length: 0",
        ),
    ] {
        assert!(
            action_input(&config, &headers, &body).is_err(),
            "headers: {headers}"
        );
    }
    for (key, value) in [
        ("model", json!("stale-model")),
        ("action", json!("promote")),
        ("experiment", json!("missing")),
        ("name", json!("../outside")),
        ("changes", json!([{ "tensor": "x", "scale": 4.01 }])),
        ("changes", json!([{ "tensor": "x", "scale": "NaN" }])),
        ("changes", json!([])),
    ] {
        let mut input = input(&config);
        input[key] = value;
        let body = input.to_string();
        assert!(
            action_input(&config, &headers(&body), &body).is_err(),
            "field: {key}"
        );
    }
    assert!(action_input(&config, &headers("{"), "{").is_err());
}

#[test]
fn running_job_rejects_duplicate_and_status_reports_outcome() {
    let (_dir, config) = config();
    let job = Arc::new(parking_lot::Mutex::new(json!({"running": true})));
    let body = input(&config).to_string();
    let request = format!("{}\r\n\r\n{body}", headers(&body));
    assert_eq!(respond(&config, &job, &request).0, "409 Conflict");
    *job.lock() = json!({"running": false, "error": "worker unavailable"});
    let (code, _, body) = respond(&config, &job, "GET /api/status HTTP/1.1\r\n\r\n");
    assert_eq!(code, "200 OK");
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["dashboard_job"]["error"], "worker unavailable");
    assert_eq!(
        respond(&config, &job, "GET /api/scale HTTP/1.1\r\n\r\n").0,
        "404 Not Found"
    );
    assert_eq!(
        respond(&config, &job, "OPTIONS /api/scale HTTP/1.1\r\n\r\n").0,
        "405 Method Not Allowed"
    );
}

#[test]
fn accepted_job_reports_real_owner_failure_without_writing_a_copy() {
    let (_dir, config) = config();
    let job = Arc::new(parking_lot::Mutex::new(Value::Null));
    let body = input(&config).to_string();
    let request = format!("{}\r\n\r\n{body}", headers(&body));
    assert_eq!(respond(&config, &job, &request).0, "202 Accepted");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while job.lock()["running"] == true {
        assert!(
            std::time::Instant::now() < deadline,
            "weight owner did not return"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(
        job.lock()["error"].as_str().is_some(),
        "default hosted model must be refused"
    );
    assert!(
        !crate::weights::directory(&config)
            .join("edits/scaled")
            .exists()
    );
}
