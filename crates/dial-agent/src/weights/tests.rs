use super::*;

fn mlx_checkpoint(root: &Path) -> PathBuf {
    let model = root.join("model");
    std::fs::create_dir(&model).unwrap();
    std::fs::write(model.join("config.json"), r#"{"model_type":"llama"}"#).unwrap();
    std::fs::write(model.join("model.safetensors"), "x").unwrap();
    model
}

fn config_for(root: &Path, model: &Path) -> Config {
    Config {
        state_dir: root.join("state"),
        user_dir: root.join("user"),
        python: PathBuf::from("/usr/bin/python3"),
        worker: root.join("worker.py"),
        model: model.to_string_lossy().into_owned(),
        worker_timeout_secs: 60,
        ..Config::default()
    }
}

#[test]
fn operations_round_trip_and_unknown_names_are_refused() {
    for op in Operation::ALL {
        assert_eq!(Operation::parse(op.name()).unwrap(), op);
    }
    assert!(Operation::parse("train").is_err());
    assert!(!Operation::Map.loads_model() && !Operation::Stats.loads_model());
    assert!(Operation::Activations.loads_model() && Operation::Trace.loads_model());
    assert!(Operation::Generate.loads_model() && Operation::Direction.loads_model());
    // An edit rewrites tensors without a forward pass: it belongs to an experiment but is free.
    assert!(!Operation::Edit.loads_model() && Operation::Edit.budgeted());
    assert!(!Operation::Trace.budgeted() && Operation::Generate.budgeted());
}

#[test]
fn arguments_keep_only_validated_worker_fields() {
    let pair = json!({"positive":[{"role":"user","content":"q"},{"role":"assistant","content":"a"}],
                      "negative":[{"role":"user","content":"q"},{"role":"assistant","content":"b"}]});
    let case =
        json!({"messages":[{"role":"user","content":"q"},{"role":"assistant","content":"a"}]});
    let cases: &[(Operation, Value, Result<Value, &str>)] = &[
        (Operation::Map, json!({"model":"/elsewhere"}), Ok(json!({}))),
        (Operation::Map, json!([1]), Err("JSON object")),
        (
            Operation::Stats,
            json!({"prefix":"model.layers.3"}),
            Ok(json!({"prefix":"model.layers.3"})),
        ),
        (
            Operation::Stats,
            json!({"prefix":"x".repeat(513)}),
            Err("at most 512"),
        ),
        (Operation::Activations, json!({}), Err("pairs")),
        (Operation::Activations, json!({"pairs":[]}), Err("1–64")),
        (
            Operation::Activations,
            json!({"pairs":[{"positive":[]}]}),
            Err("pair 0"),
        ),
        (
            Operation::Activations,
            json!({"pairs":[pair.clone()]}),
            Ok(json!({"pairs":[pair]})),
        ),
        (
            Operation::Trace,
            json!({"cases":[{"messages":[{"role":"user","content":"q"}]}]}),
            Err("case 0"),
        ),
        (
            Operation::Trace,
            json!({"cases":[case.clone()]}),
            Ok(json!({"cases":[case.clone()],"mode":"layers"})),
        ),
        (
            Operation::Trace,
            json!({"cases":[case.clone()],"layers":[0,2]}),
            Ok(json!({"cases":[case.clone()],"mode":"layers","layers":[0,2]})),
        ),
        (
            Operation::Trace,
            json!({"cases":[case.clone()],"layers":[-1]}),
            Err("zero-based"),
        ),
        (
            Operation::Trace,
            json!({"cases":[case.clone()],"mode":"heads"}),
            Err("layer"),
        ),
        (
            Operation::Trace,
            json!({"cases":[case.clone()],"mode":"heads","layer":3}),
            Ok(json!({"cases":[case.clone()],"mode":"heads","layer":3})),
        ),
        (
            Operation::Trace,
            json!({"cases":[case.clone()],"mode":"neurons"}),
            Err("layers or heads"),
        ),
        (
            Operation::Generate,
            json!({"cases":[{"messages":[{"role":"user","content":"q"}]}],"max_tokens":64,"tail":10,"experiment":"x"}),
            Ok(
                json!({"cases":[{"messages":[{"role":"user","content":"q"}]}],"max_tokens":64,"tail":10}),
            ),
        ),
        (
            Operation::Generate,
            json!({"cases":[case.clone()]}),
            Err("must end with the user turn"),
        ),
        (
            Operation::Generate,
            json!({"cases":[{"messages":[{"role":"user","content":"q"}]}],"max_tokens":5000}),
            Err("max_tokens"),
        ),
        (
            Operation::Direction,
            json!({"positive":[case.clone()],"negative":[case.clone()],"name":"loop"}),
            Ok(
                json!({"positive":[case.clone()],"negative":[case.clone()],"site":"mlp","name":"loop"}),
            ),
        ),
        (
            Operation::Direction,
            json!({"positive":[case.clone()],"negative":[case.clone()],"name":"loop","site":"attn"}),
            Err("site"),
        ),
        (
            Operation::Direction,
            json!({"positive":[case.clone()],"negative":[case.clone()],"name":"../x"}),
            Err("name must be"),
        ),
        (
            Operation::Edit,
            json!({"name":"e1","direction":"loop","layers":[3,4],"columns":32,"alpha":0.5}),
            Ok(
                json!({"method":"project_out","name":"e1","direction":"loop","layers":[3,4],"columns":32,"alpha":0.5}),
            ),
        ),
        (
            Operation::Edit,
            json!({"name":"e1","direction":"loop","layers":[3,3]}),
            Err("distinct"),
        ),
        (
            Operation::Edit,
            json!({"name":"e1","direction":"loop","layers":[3],"alpha":3.0}),
            Err("alpha"),
        ),
    ];
    for (op, input, expected) in cases {
        let actual = arguments(*op, input);
        match expected {
            Ok(value) => assert_eq!(&actual.unwrap(), value, "{op:?} {input}"),
            Err(text) => {
                let error = format!("{:#}", actual.unwrap_err());
                assert!(error.contains(text), "{op:?} {input}: {error}");
            }
        }
    }
}

#[test]
fn target_refuses_hosted_and_non_mlx_models() {
    let root = tempfile::tempdir().unwrap();
    let model = mlx_checkpoint(root.path());
    let mut config = config_for(root.path(), &model);
    assert_eq!(
        target(&config).unwrap(),
        std::fs::canonicalize(&model).unwrap()
    );

    config.provider = Some("openai".into());
    assert!(format!("{:#}", target(&config).unwrap_err()).contains("Dial-owned local MLX"));

    let gguf = root.path().join("model.gguf");
    let mut bytes = b"GGUF".to_vec();
    bytes.extend_from_slice(&[0; 60]);
    std::fs::write(&gguf, bytes).unwrap();
    let config = config_for(root.path(), &gguf);
    let error = format!("{:#}", target(&config).unwrap_err());
    assert!(
        error.contains("GGUF") || error.contains("no complete downloaded weights"),
        "{error}"
    );
}

#[test]
fn bounded_caps_tensor_rows_but_keeps_aggregates() {
    let rows: Vec<Value> = (0..MAX_TENSOR_ROWS + 3)
        .map(|n| json!({"name": n}))
        .collect();
    let value = bounded(json!({"tensors": rows, "layers": [{"layer": 0}]}));
    assert_eq!(value["tensors"].as_array().unwrap().len(), MAX_TENSOR_ROWS);
    assert_eq!(value["tensors_total"], MAX_TENSOR_ROWS + 3);
    assert_eq!(value["tensors_truncated"], true);
    assert_eq!(value["layers"], json!([{"layer": 0}]));

    let small = bounded(json!({"tensors": [{"name": "a"}]}));
    assert!(small.get("tensors_truncated").is_none());
    // Results without tensor rows (activations, trace) gain no `tensors` key.
    assert!(bounded(json!({"heads": []})).get("tensors").is_none());
}

#[test]
fn bars_scale_to_the_largest_magnitude_and_mark_missing_values() {
    let rows = [
        json!({"layer": 0, "delta": 2.0}),
        json!({"layer": 1, "delta": -1.0}),
        json!({"layer": 2, "delta": null}),
    ];
    let lines = bars(&rows, "layer", "delta", 4);
    assert_eq!(lines[0], "   0  ████ 2.0000");
    assert_eq!(lines[1], "   1 -██·· -1.0000");
    assert_eq!(lines[2], "   2  n/a");
}

#[test]
fn render_summarises_each_operation() {
    let layers = json!([{"layer": 0, "params": 10, "norm": 1.5, "score": 0.2, "delta": 0.1}]);
    let cases = [
        (
            json!({"op":"map","model_id":"m","architecture":{"model_type":"qwen3","layers":1},"totals":{"tensors":3},"layers":layers}),
            "Parameters per layer",
        ),
        (
            json!({"op":"stats","model_id":"m","layers":layers}),
            "Weight norm per layer",
        ),
        (
            json!({"op":"activations","model_id":"m","pairs":8,"suggested_layer":0,"layers":layers}),
            "suggested layer 0",
        ),
        (
            json!({"op":"trace","model_id":"m","mode":"layers","baseline_loss":1.0,"layers":layers}),
            "layer is skipped",
        ),
        (
            json!({"op":"trace","model_id":"m","mode":"heads","layer":4,"heads":[{"head":0,"delta":0.3}]}),
            "Layer 4 heads",
        ),
    ];
    for (value, expected) in cases {
        let text = render(&value);
        assert!(text.starts_with("Weights "), "{text}");
        assert!(text.contains(expected), "{text}");
    }
}

#[cfg(unix)]
#[test]
fn run_sends_a_validated_request_and_stores_the_latest_result() {
    let root = tempfile::tempdir().unwrap();
    let model = mlx_checkpoint(root.path());
    let config = config_for(root.path(), &model);
    let seen = root.path().join("request.json");
    std::fs::write(
        &config.worker,
        format!(
            r#"
import json,sys
from pathlib import Path
r=json.load(sys.stdin)
Path({seen:?}).write_text(json.dumps(r))
Path(r['progress']).write_text(json.dumps({{'phase':'reading','completed':1,'total':2}}))
print(json.dumps({{'op':'stats','model':r['model'],'model_id':'fixture','prefix':r.get('prefix',''),'layers':[{{'layer':0,'norm':1.0}}]}}))
"#,
            seen = seen.to_str().unwrap()
        ),
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let value = run(
        &config,
        None,
        Operation::Stats,
        &json!({"prefix": "model.layers.0", "ignored": "secret prompt"}),
        None,
        &cancel,
    )
    .unwrap();
    let request: Value = serde_json::from_str(&std::fs::read_to_string(&seen).unwrap()).unwrap();
    assert_eq!(request["op"], "weights_stats");
    assert_eq!(request["prefix"], "model.layers.0");
    assert_eq!(
        request["model"],
        std::fs::canonicalize(&model).unwrap().to_str().unwrap()
    );
    assert!(request.get("ignored").is_none());
    assert!(value["created_at"].as_u64().is_some());

    assert_eq!(
        latest(&config, Operation::Stats).unwrap(),
        Some(value.clone())
    );
    assert_eq!(latest(&config, Operation::Trace).unwrap(), None);
    let status = status(&config);
    assert_eq!(status["stats"], value);
    assert!(status["map"].is_null());
    assert!(
        status["progress"].is_null(),
        "finished jobs leave no progress"
    );
    assert!(status["target"]["path"].is_string());
}

#[test]
fn dashboard_serves_the_page_and_stored_status_only() {
    let root = tempfile::tempdir().unwrap();
    let model = mlx_checkpoint(root.path());
    let config = config_for(root.path(), &model);
    let (status, kind, body) = dashboard::route(&config, "/");
    assert_eq!((status, kind), ("200 OK", crate::stats::server::HTML));
    assert!(body.contains("Dial weights"));
    let (status, _, body) = dashboard::route(&config, "/api/status?x=1");
    assert_eq!(status, "200 OK");
    let value: Value = serde_json::from_str(&body).unwrap();
    assert!(value["target"]["path"].is_string() && value["map"].is_null());
    assert_eq!(dashboard::route(&config, "/api/run").0, "404 Not Found");
    // The shared Host gate still applies in front of these routes.
    let forged = "GET /api/status HTTP/1.1\r\nHost: evil.example\r\n\r\n";
    let answer =
        crate::stats::server::answer(4100, forged, |target| dashboard::route(&config, target));
    assert_eq!(answer.0, "403 Forbidden");
}

#[test]
fn tool_is_registered_only_when_listed_and_status_needs_no_approval() {
    use crate::tools::integrations::ext::Registry;
    let root = tempfile::tempdir().unwrap();
    let model = mlx_checkpoint(root.path());
    let mut config = config_for(root.path(), &model);
    assert!(!Registry::standard(&config, tool::tools(&config)).contains(tool::TOOL));
    config.integrations.push(tool::TOOL.into());
    assert!(Registry::standard(&config, tool::tools(&config)).contains(tool::TOOL));

    let tool = &tool::tools(&config)[0];
    assert_eq!(tool.schema()["function"]["name"], tool::TOOL);
    assert!(!tool.needs_approval(&json!({"operation":"status"})));
    for op in Operation::ALL {
        assert!(
            tool.needs_approval(&json!({"operation": op.name()})),
            "{op:?}"
        );
        assert!(!tool.may_modify_workspace(&json!({"operation": op.name()})));
    }
    let cancel = AtomicBool::new(false);
    let text = tool::execute(&config, &json!({"operation":"status"}), &cancel).unwrap();
    assert!(text.contains("No results yet"), "{text}");
    let error = tool::execute(&config, &json!({"operation":"rewrite"}), &cancel).unwrap_err();
    assert!(format!("{error:#}").contains("unknown weights operation"));
    // Weight edits are charged to an experiment; the tool cannot run one without it.
    let error = tool::execute(
        &config,
        &json!({"operation":"generate","cases":[{"messages":[{"role":"user","content":"q"}]}]}),
        &cancel,
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("belong to an experiment"),
        "{error:#}"
    );
    // Promotion and revert stay with the user: the tool cannot request them.
    let operations = &tool.schema()["function"]["parameters"]["properties"]["operation"]["enum"];
    assert!(
        operations
            .as_array()
            .is_some_and(|ops| ops.contains(&json!("edit")))
    );
    for refused in ["promote", "revert"] {
        assert!(
            !operations.as_array().unwrap().contains(&json!(refused)),
            "{operations}"
        );
    }
}

#[test]
fn tool_results_cap_tensor_rows_and_keep_the_stored_total() {
    let rows: Vec<Value> = (0..tool::TOOL_TENSOR_ROWS + 5)
        .map(|n| json!({"name": n}))
        .collect();
    let value =
        tool::compact(json!({"tensors": rows, "tensors_total": 9000, "layers": [{"layer": 0}]}));
    assert_eq!(
        value["tensors"].as_array().unwrap().len(),
        tool::TOOL_TENSOR_ROWS
    );
    assert_eq!(value["tensors_total"], 9000);
    assert_eq!(value["tensors_truncated"], true);
    assert_eq!(value["layers"], json!([{"layer": 0}]));
    let small = tool::compact(json!({"tensors": [{"name": "a"}]}));
    assert!(small.get("tensors_truncated").is_none());
}

#[test]
fn render_status_reports_target_progress_and_results() {
    let status = json!({"target":{"path":"/m"}, "progress":{"phase":"measuring tensors","completed":2,"total":9},
        "map":null, "stats":{"op":"stats","model_id":"m","layers":[{"layer":0,"norm":1.0}]}, "activations":null, "trace":null});
    let text = render_status(&status);
    assert!(
        text.contains("Model: /m") && text.contains("Running: measuring tensors 2/9"),
        "{text}"
    );
    assert!(text.contains("Weight norm per layer"), "{text}");
    let none =
        render_status(&json!({"target":{"unsupported":"GGUF models are not supported yet"}}));
    assert!(
        none.contains("Unsupported model: GGUF") && none.contains("No results yet"),
        "{none}"
    );
}
