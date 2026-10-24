use super::*;

#[test]
fn gguf_and_splash_models_answer_capabilities_without_the_mlx_worker() {
    let root = tempfile::tempdir().unwrap();
    let gguf = root.path().join("m.gguf");
    std::fs::write(&gguf, b"GGUF\x03\0\0\0rest").unwrap();
    let caps = non_mlx_capabilities(gguf.to_str().unwrap()).unwrap();
    assert_eq!(caps["lora"], false);
    assert_eq!(
        caps["inference"],
        crate::llm::local_format::llama_server().is_some()
    );
    validate_capabilities(caps).unwrap();

    let splash = root.path().join("splash");
    for dir in ["target", "tokenizer"] {
        std::fs::create_dir_all(splash.join(dir)).unwrap();
    }
    std::fs::write(splash.join("manifest.json"), "{}").unwrap();
    let caps = non_mlx_capabilities(splash.to_str().unwrap()).unwrap();
    assert_eq!(caps["inference"], false);
    assert!(caps["reason"].as_str().unwrap().contains("LM Studio"));
    validate_capabilities(caps).unwrap();

    let mlx = root.path().join("mlx");
    std::fs::create_dir(&mlx).unwrap();
    std::fs::write(mlx.join("config.json"), "{}").unwrap();
    std::fs::write(mlx.join("model.safetensors"), b"x").unwrap();
    assert!(non_mlx_capabilities(mlx.to_str().unwrap()).is_none());
}

#[cfg(windows)]
#[test]
fn windows_handoff_delivers_stdin_and_drains_output() {
    let output = run_command(
        Path::new("cmd.exe"),
        &["/C".into(), "more".into()],
        "worker input\n",
        10,
    )
    .unwrap();
    assert_eq!(output, "worker input\n");
}

#[cfg(unix)]
#[test]
fn training_exits_before_coding_and_resumes_after_compute_is_released() {
    handoff_fixture(false);
}
#[cfg(unix)]
#[test]
fn unresponsive_training_is_preempted_and_retries_its_committed_checkpoint() {
    handoff_fixture(true);
}
#[cfg(unix)]
#[test]
fn fuse_publishes_a_checkpoint_and_validates_the_worker_reply() {
    let root = tempfile::tempdir().unwrap();
    let base = root.path().join("base");
    std::fs::create_dir(&base).unwrap();
    std::fs::write(base.join("config.json"), "{}").unwrap();
    std::fs::write(base.join("model.safetensors"), "x").unwrap();
    let adapter = root.path().join("adapter");
    let request = root.path().join("request.json");
    let config = Config {
        state_dir: root.path().join("state"),
        user_dir: root.path().join("user"),
        python: PathBuf::from("/usr/bin/python3"),
        worker: root.path().join("worker.py"),
        model: "fixture".into(),
        worker_timeout_secs: 60,
        ..Config::default()
    };
    std::fs::write(
        &config.worker,
        format!(
            r#"
import json,sys
from pathlib import Path
r=json.load(sys.stdin)
Path({request:?}).write_text(json.dumps(r))
out=Path(r['output'])
out.mkdir()
(out/'config.json').write_text('{{}}')
if out.name != 'empty':
    (out/'model.safetensors').write_text('x')
print(json.dumps({{'output':str(out),'model_type':'llama','layers':4,'dequantized':r['dequantize']}}))
"#,
            request = request.to_str().unwrap()
        ),
    )
    .unwrap();
    let base_name = base.to_str().unwrap();

    let output = root.path().join("twin");
    let fused = fuse(&config, base_name, &adapter, &output, false).unwrap();
    assert_eq!(
        fused.output.canonicalize().unwrap(),
        output.canonicalize().unwrap()
    );
    assert_eq!((fused.layers, fused.dequantized), (4, false));
    let sent: Value = serde_json::from_str(&std::fs::read_to_string(&request).unwrap()).unwrap();
    assert_eq!(sent["op"], "fuse");
    assert_eq!(sent["dequantize"], false);
    assert_eq!(sent["adapter"], adapter.to_str().unwrap());
    let full = fuse(
        &config,
        base_name,
        &adapter,
        &root.path().join("full"),
        true,
    )
    .unwrap();
    assert!(full.dequantized);

    let error = fuse(&config, base_name, &adapter, &output, false).unwrap_err();
    assert!(error.to_string().contains("already exists"), "{error}");
    let error = fuse(
        &config,
        base_name,
        &adapter,
        &root.path().join("empty"),
        false,
    )
    .unwrap_err();
    assert!(error.to_string().contains("did not produce"), "{error}");
    let gguf_base = root.path().join("m.gguf");
    std::fs::write(&gguf_base, b"GGUF\x03\0\0\0").unwrap();
    let error = fuse(
        &config,
        gguf_base.to_str().unwrap(),
        &adapter,
        &root.path().join("g"),
        false,
    )
    .unwrap_err();
    assert!(error.to_string().contains("MLX base"), "{error}");
}

#[cfg(unix)]
fn handoff_fixture(unresponsive: bool) {
    use fs2::FileExt;
    let root = tempfile::tempdir().unwrap();
    let mut config = Config {
        state_dir: root.path().join("state"),
        user_dir: root.path().join("user"),
        python: PathBuf::from("/usr/bin/python3"),
        worker: root.path().join("worker.py"),
        model: "fixture".into(),
        use_case: "project".into(),
        worker_timeout_secs: 60,
        ..Config::default()
    };
    config.training.enabled = true;
    let started = root.path().join("started");
    let coded = root.path().join("coded");
    std::fs::write(
        &config.worker,
        format!(
            r#"
import json,sys,time
from pathlib import Path
r=json.load(sys.stdin)
out=Path(r['output_dir'])
out.mkdir(exist_ok=True)
if not r['resume_checkpoint']:
    Path({started:?}).touch()
    if {unresponsive}:
        (out/'resume.json').write_text('{{"completed":2}}')
        time.sleep(30)
    deadline=time.monotonic()+60
    while not Path(r['pause_file']).exists():
        assert time.monotonic()<deadline
        time.sleep(0.01)
    (out/'resume.json').write_text('{{"completed":2}}')
    print(json.dumps({{'paused':True}}))
else:
    assert Path({coded:?}).exists(), 'training resumed before coding completed'
    for name in ['adapter_config.json','adapters.safetensors']:
        (out/name).write_text('fixture')
    print(json.dumps({{'adapter':str(out.resolve())}}))
"#,
            started = started.to_str().unwrap(),
            coded = coded.to_str().unwrap(),
            unresponsive = if unresponsive { "True" } else { "False" },
        ),
    )
    .unwrap();
    let store = crate::storage::store::Store::open(
        &config.state_dir,
        &config.model,
        &config.use_case,
        &config.harness,
    )
    .unwrap();
    let active = store.active().unwrap();
    let _lease = super::super::compute::Background::acquire(&config).unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let deadline = Instant::now() + Duration::from_secs(60);
            while !started.exists() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            let request = super::super::compute::Request::foreground(&config.user_dir).unwrap();
            let lock =
                super::super::auto::lock_file(&config.user_dir.join("compute.lock")).unwrap();
            while fs2::FileExt::try_lock_shared(&lock).is_err() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            drop(request);
            let reader = crate::storage::store::Store::open_read_only(&config.state_dir).unwrap();
            assert_eq!(
                reader.get_meta("lora_phase").unwrap().as_deref(),
                Some("paused")
            );
            std::thread::sleep(Duration::from_millis(100));
            std::fs::write(&coded, "").unwrap();
            FileExt::unlock(&lock).unwrap();
        });
        let output = root.path().join("adapter");
        assert_eq!(
            train(&config, &active, &output).unwrap(),
            output.canonicalize().unwrap()
        );
    });
    assert!(
        super::super::compute::can_yield(),
        "the resumed cycle must own compute before evaluating"
    );
    assert_eq!(
        store.active().unwrap().id,
        active.id,
        "checkpointing must not activate a candidate"
    );
}

#[cfg(unix)]
#[test]
fn stop_request_terminates_running_training_subprocess() {
    let root = tempfile::tempdir().unwrap();
    let stop = root.path().join("improvement.stop");
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(100));
            std::fs::write(&stop, "").unwrap();
        });
        let started = Instant::now();
        let error = run_command_controlled(
            Path::new("/bin/sh"),
            &["-c".into(), "sleep 30".into()],
            "",
            10,
            None,
            Some(&stop),
        )
        .unwrap_err();
        assert!(error.to_string().contains("stopped by operator"));
        assert!(started.elapsed() < Duration::from_secs(3));
    });
}

#[cfg(unix)]
#[test]
fn lora_verification_cancellation_stops_the_worker_promptly() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(40));
            cancel.store(true, Ordering::SeqCst);
        });
        let error = run_command_controlled(
            Path::new("/bin/sh"),
            &["-c".into(), "sleep 30".into()],
            "",
            10,
            Some(&cancel),
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
    });
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn lora_probe_validates_supported_and_unsupported_results() {
    let root = tempfile::tempdir().unwrap();
    let mut caps = json!({"inference":true,"lora":true,"resolved_model":root.path(),"reason":"supported query targets",
        "unsupported":["expert_growth","routing_calibration","attention_retrofit","learned_step_probe"]});
    assert!(validate_capabilities(caps.clone()).is_ok());
    caps["lora"] = json!(false);
    caps["reason"] = json!("fused QKV unsupported");
    assert_eq!(
        validate_capabilities(caps.clone()).unwrap()["reason"],
        "fused QKV unsupported"
    );
    caps["inference"] = json!(false);
    caps["lora"] = json!(true);
    assert!(validate_capabilities(caps).is_err());
}

#[test]
fn choose_layers_takes_smallest_covering_suffix() {
    let scores = [1.0, 1.0, 2.0, 6.0];
    assert_eq!(choose_layers(&scores, 0.5, 1, 4), 1);
    assert_eq!(choose_layers(&scores, 0.6, 1, 4), 1);
    assert_eq!(choose_layers(&scores, 0.61, 1, 4), 2);
    assert_eq!(choose_layers(&scores, 0.8, 1, 4), 2);
    assert_eq!(choose_layers(&scores, 0.81, 1, 4), 3);
    assert_eq!(choose_layers(&scores, 1.0, 1, 4), 4);
}

#[test]
fn choose_layers_is_monotonic_in_coverage() {
    let scores = [3.0, 0.5, 2.0, 0.0, 1.5, 4.0, 0.1];
    let mut previous = 0;
    for step in 0..=100 {
        let chosen = choose_layers(&scores, f64::from(step) / 100.0, 1, 7);
        assert!(
            chosen >= previous,
            "coverage {step}% chose {chosen} after {previous}"
        );
        previous = chosen;
    }
    assert_eq!(previous, 7);
}

#[test]
fn choose_layers_clamps_to_bounds_and_layer_count() {
    let scores = [1.0, 1.0, 1.0, 1.0];
    assert_eq!(choose_layers(&scores, 0.0, 3, 4), 3);
    assert_eq!(choose_layers(&scores, 1.0, 1, 2), 2);
    assert_eq!(choose_layers(&scores, 1.0, 1, 100), 4);
    assert_eq!(choose_layers(&scores, 0.1, 10, 100), 4);
    assert_eq!(choose_layers(&scores, 1.0, 5, 2), 2);
    assert_eq!(choose_layers(&scores, 7.0, 1, 4), 4);
    assert_eq!(choose_layers(&scores, 0.0, 0, 4), 1);
    assert_eq!(choose_layers(&scores, 1.0, 0, 0), 1);
}

#[test]
fn choose_layers_degenerate_input_returns_min() {
    assert_eq!(choose_layers(&[], 0.9, 2, 8), 0);
    assert_eq!(choose_layers(&[0.0, 0.0, 0.0], 0.9, 2, 8), 2);
    assert_eq!(
        choose_layers(&[f64::NAN, f64::INFINITY, -1.0], 0.9, 2, 8),
        2
    );
    assert_eq!(choose_layers(&[0.0, 0.0], 0.9, 5, 8), 2);
    assert_eq!(choose_layers(&[1.0, 2.0], f64::NAN, 1, 2), 1);
}

#[test]
fn choose_layers_ignores_nonfinite_scores_in_mass() {
    assert_eq!(choose_layers(&[5.0, f64::NAN, 5.0], 0.5, 1, 3), 1);
}

#[test]
#[ignore = "needs python/.venv and the cached Qwen2.5-0.5B-Instruct-4bit model"]
fn real_sensitivity_scores_every_layer() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let python = root.join("python/.venv/bin/python");
    let worker = root.join("python/dial_worker.py");
    let data = std::env::temp_dir().join(format!("dial-sensitivity-{}.jsonl", std::process::id()));
    let mut lines = String::new();
    for n in 0..6 {
        let record = json!({"messages": [{"role": "user", "content": format!("What is {n} + 3?")},
            {"role": "assistant", "content": format!("The answer is {}.", n + 3)}],
            "source": "test", "verified": true});
        lines.push_str(&format!("{record}\n"));
    }
    std::fs::write(&data, lines).unwrap();
    let config = Config {
        python,
        worker,
        worker_timeout_secs: 600,
        ..Config::default()
    };
    let result = sensitivity(
        &config,
        "mlx-community/Qwen2.5-0.5B-Instruct-4bit",
        &data,
        8,
        9,
        512,
    );
    let _ = std::fs::remove_file(&data);
    let scores = result.unwrap();
    assert_eq!(scores.len(), 24);
    assert!(
        scores.iter().all(|score| score.is_finite() && *score > 0.0),
        "{scores:?}"
    );
    println!("{scores:?}");
    let chosen = choose_layers(&scores, 0.5, 2, 24);
    println!("chosen suffix: {chosen}");
    assert!((2..=24).contains(&chosen));
}

#[cfg(unix)]
#[test]
fn budget_expiry_stops_and_reaps_controlled_workers() {
    for mode in ["request", "cooperative", "unresponsive"] {
        let root = tempfile::tempdir().unwrap();
        let mut config = Config {
            state_dir: root.path().join("state"),
            user_dir: root.path().join("user"),
            python: PathBuf::from("/usr/bin/python3"),
            worker: root.path().join("worker.py"),
            model: "fixture".into(),
            use_case: "budget lifecycle regression".into(),
            worker_timeout_secs: 30,
            ..Config::default()
        };
        config.improve.daily_compute_minutes = 1;
        config.training.enabled = true;
        let marker = root.path().join("started");
        std::fs::write(
            &config.worker,
            format!(
                r#"
import json,sys,time,os
from pathlib import Path
r=json.load(sys.stdin)
Path({marker:?}).write_text(str(os.getpid()))
if r['op']=='train':
    out=Path(r['output_dir'])
    out.mkdir()
    (out/'resume.json').write_text('{{"completed":1}}')
    while not Path(r['pause_file']).exists():
        time.sleep(0.005)
    if {cooperative}:
        print(json.dumps({{'paused':True}}))
        sys.exit(0)
time.sleep(30)
"#,
                marker = marker.to_str().unwrap(),
                cooperative = if mode == "cooperative" {
                    "True"
                } else {
                    "False"
                }
            ),
        )
        .unwrap();
        let store = crate::storage::store::Store::open(
            &config.state_dir,
            &config.model,
            &config.use_case,
            &config.harness,
        )
        .unwrap();
        let active = store.active().unwrap();
        let lease = super::super::compute::Background::acquire(&config).unwrap();
        let started = Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                while !marker.exists() {
                    assert!(started.elapsed() < Duration::from_secs(5));
                    std::thread::sleep(Duration::from_millis(5));
                }
                super::super::resources::record_usage(&config.user_dir, 60).unwrap();
            });
            let result = if mode == "request" {
                call(&config, &json!({"op":"generate"})).map(|_| ())
            } else {
                train(&config, &active, &root.path().join("adapter")).map(|_| ())
            };
            let error = result.unwrap_err();
            assert!(
                error
                    .downcast_ref::<super::super::auto::BudgetReached>()
                    .is_some(),
                "{error:#}"
            );
        });
        assert!(started.elapsed() < Duration::from_secs(5));
        let pid: u32 = std::fs::read_to_string(marker).unwrap().parse().unwrap();
        assert!(
            !crate::platform::process::alive(pid),
            "worker was not reaped"
        );
        if mode != "request" {
            assert!(root.path().join("adapter/resume.json").is_file());
        }
        drop(lease);
        assert!(!super::super::compute::background_active());
        let file = super::super::auto::lock_file(&config.user_dir.join("compute.lock")).unwrap();
        fs2::FileExt::try_lock_exclusive(&file).unwrap();
    }
}
