use super::*;
use crate::config::types::{Admission, SuiteScore};

fn settings() -> Settings {
    Settings {
        context_tokens: 4096,
        lora_min_examples: 24,
        min_new_turns: 5,
        cycle_interval_secs: 3600,
    }
}

fn layout(root: &Path) -> Layout {
    Layout {
        project_root: root.join("project"),
        project_dir: root.join("project/.dial"),
        user_dir: root.join("user"),
        state_dir: root.join("state"),
    }
}

fn write_mlx(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    fs::write(
        dir.join("config.json"),
        r#"{"model_type":"llama","num_hidden_layers":2,"num_attention_heads":4,"num_key_value_heads":4,"hidden_size":64,"max_position_embeddings":8192}"#,
    )
    .unwrap();
    fs::write(dir.join("model.safetensors"), vec![0u8; 4096]).unwrap();
}

#[test]
fn speed_summary_medians_and_trend_are_exact() {
    // Forty samples: the older twenty at 10 tok/s, the newest twenty at 20 tok/s.
    let rising: Vec<f64> = (0..40).map(|i| if i < 20 { 10.0 } else { 20.0 }).collect();
    let speed = stats::speed_summary(&rising);
    assert_eq!(speed.samples, Some(40));
    assert_eq!(speed.median_tok_per_s, Some(15.0));
    assert_eq!(speed.recent_median_tok_per_s, Some(20.0));
    assert_eq!(speed.previous_median_tok_per_s, Some(10.0));
    assert_eq!(speed.trend, Some("up"));
    assert_eq!(speed.history.len(), 40);

    let short = stats::speed_summary(&[5.0, 7.0]);
    assert_eq!(short.median_tok_per_s, Some(6.0));
    assert_eq!(short.previous_median_tok_per_s, None);
    assert_eq!(short.trend, None);
    assert_eq!(stats::speed_summary(&[]).samples, Some(0));
}

#[test]
fn missing_stats_and_stores_are_unknown_not_errors() {
    let root = tempfile::tempdir().unwrap();
    let layout = layout(root.path());
    let model = root.path().join("models/mlx-model");
    write_mlx(&model);
    let stores = learning_stores(&layout.state_dir);
    let memory = memory(&model, LocalFormat::Mlx, &settings(), Some(16 << 30));
    assert_eq!(memory.weights_bytes, Some(4096));
    assert!(memory.resident_bytes.is_some());
    assert_eq!(memory.context_tokens, Some(4096));
    let improvement = improvement(&stores, &["mlx-model"], &settings(), 1_000);
    assert_eq!(improvement.store, "no_store");
    assert_eq!(improvement.verified_turns, None);
    assert_eq!(improvement.version, None);
    assert!(improvement.store_dir.is_none());
    assert!(
        !layout.state_dir.join("models").exists(),
        "reading must not create stores"
    );
    assert!(!stats::db_path(&layout.user_dir).exists());
    assert!(stats_db(&layout.user_dir).unwrap().is_none());
    assert_eq!(DecodeSpeed::default().samples, None);
    assert_eq!(
        speed_text(&DecodeSpeed::default()),
        "unknown (stats unavailable)"
    );
}

#[test]
fn unknown_memory_reports_unknown_fit_and_gguf_is_sized_from_its_header() {
    use crate::llm::local_format::gguf_metadata_tests::{header, string, uint32};
    let root = tempfile::tempdir().unwrap();
    let model = root.path().join("m");
    write_mlx(&model);
    let no_memory = memory(&model, LocalFormat::Mlx, &settings(), None);
    assert_eq!(no_memory.fit, Fit::Unknown);
    assert!(no_memory.resident_bytes.is_some());

    let sized = root.path().join("s");
    fs::create_dir(&sized).unwrap();
    let bytes = header(&[
        ("general.architecture", string("llama")),
        ("llama.block_count", uint32(2)),
        ("llama.attention.head_count", uint32(4)),
        ("llama.attention.head_count_kv", uint32(4)),
        ("llama.embedding_length", uint32(64)),
        ("llama.context_length", uint32(8192)),
    ]);
    fs::write(sized.join("model-Q8_0.gguf"), &bytes).unwrap();
    let row = memory(&sized, LocalFormat::Gguf, &settings(), Some(16 << 30));
    assert_eq!(row.weights_bytes, Some(bytes.len() as u64));
    assert!(
        row.resident_bytes
            .is_some_and(|resident| resident > bytes.len() as u64)
    );
    assert_eq!(row.fit, Fit::Fits);

    // A header without a transformer layout still reports the file size, but no estimate.
    let gguf = root.path().join("g");
    fs::create_dir(&gguf).unwrap();
    fs::write(gguf.join("model-Q4.gguf"), b"GGUF\x03\0\0\0rest").unwrap();
    let row = memory(&gguf, LocalFormat::Gguf, &settings(), Some(16 << 30));
    assert_eq!(row.fit, Fit::Unknown);
    assert_eq!(row.resident_bytes, None);
    assert_eq!(row.weights_bytes, Some(12));
}

#[test]
fn store_reads_adapter_gate_and_what_is_waiting() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let dir = state.join("models/abc");
    fs::create_dir_all(&dir).unwrap();
    let conn = Connection::open(dir.join("state.sqlite3")).unwrap();
    conn.execute_batch(
        "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE versions(id INTEGER PRIMARY KEY, parent INTEGER, kind TEXT NOT NULL,
               harness TEXT NOT NULL, adapter TEXT, status TEXT NOT NULL);
             CREATE TABLE admissions(id INTEGER PRIMARY KEY, candidate INTEGER NOT NULL,
               baseline INTEGER NOT NULL, report TEXT NOT NULL, activated INTEGER NOT NULL,
               decision TEXT NOT NULL);
             CREATE TABLE trajectories(id INTEGER PRIMARY KEY, version INTEGER NOT NULL,
               domain TEXT NOT NULL, finished_at TEXT);
             CREATE TABLE outcomes(trajectory INTEGER PRIMARY KEY, score REAL NOT NULL,
               source TEXT NOT NULL);
             INSERT INTO meta VALUES('model', 'mlx-model'), ('use_case', 'coding'),
               ('last_cycle_trajectory', '10'), ('last_cycle_attempt', '1000');
             INSERT INTO versions VALUES(1, NULL, 'harness', '{}', NULL, 'known_good'),
               (2, 1, 'lora', '{}', '/adapters/2', 'active');",
    )
    .unwrap();
    let report = Admission {
        candidate: 2,
        baseline: 1,
        passed: true,
        reason: "ok".into(),
        scores: vec![SuiteScore {
            name: "primary".into(),
            primary: true,
            baseline: 50.0,
            candidate: 62.5,
            cases: 8,
            candidate_only: 2,
            baseline_only: 0,
            tier: None,
        }],
    };
    conn.execute(
        "INSERT INTO admissions VALUES(1, 2, 1, ?1, 1, 'activated')",
        [serde_json::to_string(&report).unwrap()],
    )
    .unwrap();
    // Ten finished live turns after the watermark: three verified, the rest unverified.
    for id in 11..=20 {
        conn.execute(
            "INSERT INTO trajectories VALUES(?1, 2, 'live', '2026-01-01')",
            [id],
        )
        .unwrap();
    }
    for id in 11..=13 {
        conn.execute("INSERT INTO outcomes VALUES(?1, 100, 'user')", [id])
            .unwrap();
    }
    drop(conn);

    let stores = learning_stores(&state);
    // Cooldown: attempt at 1000 + 3600 interval, now 2000 → 2600 s left.
    let found = improvement(&stores, &["mlx-model"], &settings(), 2_000);
    assert_eq!(found.store, "found");
    assert_eq!(found.version, Some(2));
    assert_eq!(found.kind.as_deref(), Some("lora"));
    assert_eq!(found.adapter.as_deref(), Some("/adapters/2"));
    assert_eq!(found.verified_turns, Some(3));
    assert_eq!(found.new_turns, Some(10));
    assert_eq!(found.cooldown_secs, Some(2600));
    let gate = found.last_gate.as_ref().unwrap();
    assert_eq!(gate.primary_candidate, Some(62.5));
    assert!(gate.passed && gate.activated);
    let waiting = found.waiting.clone().unwrap();
    assert_eq!(
        waiting,
        vec![
            "3/24 verified turns for LoRA".to_owned(),
            "cooldown 43m 20s remaining".to_owned()
        ]
    );
    assert!(gate_text(&found).contains("primary 50.0 → 62.5"));
    assert!(version_text(&found).contains("adapter /adapters/2"));

    let other = improvement(&stores, &["unrelated"], &settings(), 2_000);
    assert_eq!(other.store, "no_store");
}

#[test]
fn unknown_fields_render_as_unknown_text() {
    let unknown = Improvement {
        store: "unreadable",
        store_dir: None,
        version: None,
        kind: None,
        adapter: None,
        last_gate: None,
        verified_turns: None,
        lora_min_examples: 24,
        new_turns: None,
        min_new_turns: 5,
        cooldown_secs: None,
        waiting: None,
    };
    assert_eq!(waiting_text(&unknown), "unknown");
    assert_eq!(
        version_text(&unknown),
        "unknown (learning store unreadable)"
    );
    assert_eq!(gate_text(&unknown), "unknown");
    let json = serde_json::to_value(&unknown).unwrap();
    assert!(json["waiting"].is_null());
    assert!(json["verified_turns"].is_null());
}

#[test]
fn dashboard_json_has_null_speed_when_stats_are_missing() {
    let root = tempfile::tempdir().unwrap();
    let layout = layout(root.path());
    let model = root.path().join("hub/mlx-model");
    write_mlx(&model);
    let stores = learning_stores(&layout.state_dir);
    let row = ModelRow {
        label: "mlx-model".into(),
        path: model.to_string_lossy().into_owned(),
        source: "test".into(),
        format: Some("MLX"),
        speed: DecodeSpeed::default(),
        memory: memory(&model, LocalFormat::Mlx, &settings(), None),
        improvement: improvement(&stores, &["mlx-model"], &settings(), 0),
    };
    let dashboard = Dashboard {
        generated_ms: 0,
        memory_bytes: None,
        memory_source: "unknown",
        stats: "missing",
        models: vec![row],
    };
    let json = serde_json::to_value(&dashboard).unwrap();
    assert_eq!(json["stats"], "missing");
    assert!(json["models"][0]["speed"]["samples"].is_null());
    assert!(json["models"][0]["speed"]["median_tok_per_s"].is_null());
    assert_eq!(json["models"][0]["memory"]["fit"], "unknown");
    assert_eq!(json["models"][0]["improvement"]["store"], "no_store");
    let text = render_text(&dashboard);
    assert!(text.contains("speed     unknown (stats unavailable)"));
    assert!(text.contains("[MLX]"));
}

#[test]
fn speed_from_recorded_requests_uses_only_successful_timed_output() {
    let root = tempfile::tempdir().unwrap();
    let mut db = stats::Db::open(&root.path().join("stats.sqlite3")).unwrap();
    let record = |ts: i64, output: u64, duration_ms: u64, error: bool| stats::RequestRecord {
        ts_ms: ts,
        session: "s".into(),
        project: "p".into(),
        model: "/models/a".into(),
        output,
        duration_ms,
        error,
        reported: true,
        finish_reason: "stop".into(),
        ..Default::default()
    };
    db.record_request(&record(1, 100, 2000, false)).unwrap();
    db.record_request(&record(2, 300, 2000, false)).unwrap();
    db.record_request(&record(3, 999, 1000, true)).unwrap();
    db.record_request(&record(4, 0, 1000, false)).unwrap();
    drop(db);
    let read = stats::Db::open_read_only(&root.path().join("stats.sqlite3")).unwrap();
    let speed = read.decode_speed(&["/models/a"]).unwrap();
    assert_eq!(speed.samples, Some(2));
    assert_eq!(speed.history, vec![50.0, 150.0]);
    assert_eq!(speed.median_tok_per_s, Some(100.0));
    assert_eq!(read.decode_speed(&["/models/b"]).unwrap().samples, Some(0));
    assert_eq!(read.model_names().unwrap(), vec!["/models/a".to_owned()]);
}
