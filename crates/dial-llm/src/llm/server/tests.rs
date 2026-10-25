use super::*;
use std::fs;

fn strings(args: Vec<std::ffi::OsString>) -> Vec<String> {
    args.into_iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

#[test]
fn gguf_launch_maps_runtime_settings_onto_llama_cpp_flags() {
    let root = tempfile::tempdir().unwrap();
    let model = root.path().join("m.gguf");
    let draft = root.path().join("draft.gguf");
    let lora = root.path().join("lora.gguf");
    for file in [&model, &draft, &lora] {
        fs::write(file, b"GGUF\x03\0\0\0").unwrap();
    }
    let projector = root.path().join("mmproj-f16.gguf");
    let launch = LlamaLaunch {
        model: &model,
        projector: Some(&projector),
        adapter: Some(&lora),
        context_tokens: Some(32768),
        shared_pool: None,
        port: 4100,
    };
    let runtime = llm::local_runtime::LocalRuntime {
        draft_model: Some(draft.to_string_lossy().into_owned()),
        num_draft_tokens: Some(5),
        kv_bits: Some(8),
        ..Default::default()
    };
    let args = strings(launch.args(&runtime).unwrap());
    let flag = |name: &str| {
        let at = args.iter().position(|arg| arg == name).unwrap();
        args[at + 1].clone()
    };
    assert_eq!(flag("-m"), model.to_string_lossy());
    assert_eq!(flag("--mmproj"), projector.to_string_lossy());
    assert_eq!(flag("--lora"), lora.to_string_lossy());
    assert_eq!(flag("--model-draft"), draft.to_string_lossy());
    // llama.cpp matches option names exactly: `--opt=value` is rejected as an unknown argument.
    assert!(
        !args
            .iter()
            .any(|arg| arg.starts_with("--") && arg.contains('=')),
        "{args:?}"
    );
    for (name, value) in [
        ("--host", "127.0.0.1"),
        ("--port", "4100"),
        ("--ctx-size", "32768"),
        ("--spec-draft-n-max", "5"),
        ("--cache-type-k", "q8_0"),
        ("--cache-type-v", "q8_0"),
        ("--parallel", "1"),
    ] {
        assert_eq!(flag(name), value, "{name} in {args:?}");
    }
    assert!(args.iter().any(|arg| arg == "--jinja"));
    assert!(!args.iter().any(|arg| arg.starts_with("--kv-unified")));

    let plain = strings(
        LlamaLaunch {
            adapter: None,
            projector: None,
            context_tokens: None,
            ..launch
        }
        .args(&Default::default())
        .unwrap(),
    );
    assert!(!plain.iter().any(|arg| arg.starts_with("--ctx-size")
        || arg.starts_with("--cache-type")
        || arg == "--lora"));
}

/// `llama-server --help` excerpts: a current build lists the shared-pool flags, an older one only
/// `--kv-unified` (or nothing).
const NEW_HELP: &str = "\
-np,   --parallel N                     number of server slots (default: -1, -1 = auto)
--kv-unified-per-slot N                 context limit per parallel slot (default: unset)
-kvu,  --kv-unified, -no-kvu, --no-kv-unified
                                        use single unified KV buffer shared across all sequences
--cache-idle-slots, --no-cache-idle-slots
                                        save idle slots to the prompt cache on new task
";
const OLD_HELP: &str = "\
-np,   --parallel N                     number of parallel sequences to decode (default: 1)
-kvu,  --kv-unified                     use single unified KV buffer for the KV cache of all sequences
";

#[test]
fn shared_slots_need_every_flag_as_a_whole_option() {
    assert!(help_lists_shared_slots(NEW_HELP));
    assert!(!help_lists_shared_slots(OLD_HELP));
    assert!(!help_lists_shared_slots(""));
    // `--kv-unified-per-slot` alone must not count as `--kv-unified`.
    assert!(!help_lists_shared_slots(
        "--kv-unified-per-slot N\n--no-cache-idle-slots\n"
    ));
}

#[test]
fn shared_pool_adds_a_bounded_side_reserve_only_when_supported_and_affordable() {
    assert_eq!(side_reserve(8192), 4096);
    assert_eq!(side_reserve(32768), 8192);
    assert_eq!(side_reserve(262144), 16384);
    assert_eq!(side_reserve(2048), 2048, "never more than the window");
    assert_eq!(shared_pool(Some(32768), true, |_| true), Some(40960));
    assert_eq!(shared_pool(Some(32768), false, |_| true), None, "old build");
    assert_eq!(shared_pool(None, true, |_| true), None, "unknown window");
    assert_eq!(
        shared_pool(Some(32768), true, |_| false),
        None,
        "a reserve that does not fit never shrinks the main window"
    );
    let mut asked = None;
    shared_pool(Some(8192), true, |pool| {
        asked = Some(pool);
        true
    });
    assert_eq!(asked, Some(12288));
}

#[test]
fn shared_launch_keeps_the_whole_window_per_slot_over_one_pool() {
    let root = tempfile::tempdir().unwrap();
    let model = root.path().join("m.gguf");
    fs::write(&model, b"GGUF\x03\0\0\0").unwrap();
    let launch = LlamaLaunch {
        model: &model,
        projector: None,
        adapter: None,
        context_tokens: Some(32768),
        shared_pool: shared_pool(Some(32768), help_lists_shared_slots(NEW_HELP), |_| true),
        port: 4100,
    };
    let args = strings(launch.args(&Default::default()).unwrap());
    let flag = |name: &str| {
        let at = args.iter().position(|arg| arg == name).unwrap();
        args[at + 1].clone()
    };
    assert_eq!(flag("--parallel"), LLAMA_SHARED_SLOTS.to_string());
    assert_eq!(flag("--kv-unified-per-slot"), "32768");
    assert_eq!(flag("--ctx-size"), "40960");
    assert!(args.iter().any(|arg| arg == "--kv-unified"));
    assert!(args.iter().any(|arg| arg == "--no-cache-idle-slots"));
    assert_eq!(args.iter().filter(|arg| *arg == "--parallel").count(), 1);
    assert_eq!(args.iter().filter(|arg| *arg == "--ctx-size").count(), 1);

    // An old build gets exactly the single-slot arguments.
    let old = strings(
        LlamaLaunch {
            shared_pool: shared_pool(Some(32768), help_lists_shared_slots(OLD_HELP), |_| true),
            ..launch
        }
        .args(&Default::default())
        .unwrap(),
    );
    let single = strings(
        LlamaLaunch {
            shared_pool: None,
            ..launch
        }
        .args(&Default::default())
        .unwrap(),
    );
    assert_eq!(old, single);
    assert!(old.windows(2).any(|pair| pair == ["--parallel", "1"]));
    assert!(old.windows(2).any(|pair| pair == ["--ctx-size", "32768"]));
}

#[cfg(unix)]
#[test]
fn help_probe_reads_a_fake_binary_and_bounds_a_hung_one() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let script = |name: &str, body: &str| {
        let path = root.path().join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    };
    let new = script("new-server", &format!("cat <<'EOF'\n{NEW_HELP}EOF"));
    let old = script("old-server", &format!("cat <<'EOF'\n{OLD_HELP}EOF"));
    let hung = script("hung-server", "exec sleep 30");
    assert!(shared_slots_supported(&new));
    assert!(!shared_slots_supported(&old));
    assert!(!shared_slots_supported(&root.path().join("missing")));
    let started = Instant::now();
    assert_eq!(probe_help(&hung, Duration::from_millis(200)), None);
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn gguf_launch_refuses_settings_llama_cpp_cannot_honor() {
    let root = tempfile::tempdir().unwrap();
    let model = root.path().join("m.gguf");
    fs::write(&model, b"GGUF\x03\0\0\0").unwrap();
    let mlx_adapter = root.path().join("adapter");
    fs::create_dir(&mlx_adapter).unwrap();
    fs::write(mlx_adapter.join("adapters.safetensors"), b"x").unwrap();
    let launch = |adapter| LlamaLaunch {
        model: &model,
        projector: None,
        adapter,
        context_tokens: None,
        shared_pool: None,
        port: 1,
    };
    let error = launch(Some(&mlx_adapter))
        .args(&Default::default())
        .unwrap_err();
    assert!(
        error.to_string().contains("not a GGUF LoRA adapter"),
        "{error}"
    );
    for (runtime, message) in [
        (
            llm::local_runtime::LocalRuntime {
                kv_bits: Some(6),
                ..Default::default()
            },
            "no llama.cpp KV-cache type",
        ),
        (
            llm::local_runtime::LocalRuntime {
                kv_bits: Some(4),
                kv_group_size: Some(64),
                ..Default::default()
            },
            "MLX-only",
        ),
        (
            llm::local_runtime::LocalRuntime {
                draft_model: Some("mlx-community/draft".into()),
                ..Default::default()
            },
            "must be a local GGUF file",
        ),
    ] {
        let error = launch(None).args(&runtime).unwrap_err();
        assert!(error.to_string().contains(message), "{error}");
    }
}

use super::fixtures::fake_python;

fn alive(pid: i64) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

fn wait_dead(pid: i64) -> bool {
    for _ in 0..200 {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

fn info(path: &Path) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn steering_profile_change_restarts_server_and_clears_cache() {
    let root = tempfile::tempdir().unwrap();
    let python = fake_python(root.path());
    let marker = root.path().join("info.json");
    let model = marker.to_str().unwrap();
    let mut slot = None;
    let mut spec = crate::llm::steering::Specification {
        path: root.path().join("vector.json"),
        id: "a".repeat(64),
        strength: 0.5,
        fingerprint: "first".into(),
    };
    ensure_profile_controlled(&mut slot, &python, model, None, 30, None, Some(&spec)).unwrap();
    let first = info(&marker)["pid"].as_i64().unwrap();
    ensure_profile_controlled(&mut slot, &python, model, None, 30, None, Some(&spec)).unwrap();
    assert_eq!(first, info(&marker)["pid"].as_i64().unwrap());
    spec.strength = 1.;
    ensure_profile_controlled(&mut slot, &python, model, None, 30, None, Some(&spec)).unwrap();
    let second = info(&marker)["pid"].as_i64().unwrap();
    assert_ne!(first, second);
    assert!(wait_dead(first));
    ensure_profile_controlled(&mut slot, &python, model, None, 30, None, None).unwrap();
    assert_ne!(second, info(&marker)["pid"].as_i64().unwrap());
    assert!(wait_dead(second));
}

#[test]
fn ready_then_drop_kills_whole_group() {
    let dir = tempfile::tempdir().unwrap();
    let python = fake_python(dir.path());
    let marker = dir.path().join("info.json");
    let model = marker.to_str().unwrap();
    let adapter = dir.path().join("adapter");
    let server = ModelServer::start(&python, model, Some(&adapter), 30).unwrap();
    assert!(server.base_url().starts_with("http://127.0.0.1:"));
    assert!(server.matches(model, Some(&adapter)));
    assert!(!server.matches(model, None));
    assert!(!server.matches("other", Some(&adapter)));
    assert!(server.is_running());
    let started = info(&marker);
    let (pid, helper) = (
        started["pid"].as_i64().unwrap(),
        started["child"].as_i64().unwrap(),
    );
    assert!(alive(pid) && alive(helper));
    let budget = started["argv"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a.as_str()?.strip_prefix("--prompt-cache-bytes="))
        .next()
        .expect("the KV prefix cache must be bounded")
        .parse::<u64>()
        .unwrap();
    assert!(budget > 0, "{budget}");
    drop(server);
    assert!(wait_dead(pid), "server survived drop");
    assert!(wait_dead(helper), "server's child survived drop");
}

#[test]
fn explicit_stop_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let python = fake_python(dir.path());
    let marker = dir.path().join("info.json");
    let mut server = ModelServer::start(&python, marker.to_str().unwrap(), None, 30).unwrap();
    let pid = info(&marker)["pid"].as_i64().unwrap();
    server.stop();
    assert!(!server.is_running());
    assert!(!alive(pid));
    server.stop();
}

#[test]
fn early_exit_reports_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let python = fake_python(dir.path());
    let error = ModelServer::start(&python, "exit-early", None, 30)
        .err()
        .expect("must fail")
        .to_string();
    assert!(error.contains("exited during startup"), "{error}");
    assert!(error.contains("fatal: cannot load model"), "{error}");
    assert!(error.contains(python.to_str().unwrap()), "{error}");
}

#[test]
fn unknown_architecture_reports_the_selected_runtime_and_recovery_options() {
    let error = anyhow!("model server exited during startup");
    let stderr = "llama_model_load: error loading model: unknown model architecture: 'flint'\nfailed to load model";
    let program = std::ffi::OsStr::new("/custom/runtime/bin/llama-server");
    let diagnostic = startup_diagnostic(&error, stderr, program);
    for detail in [
        "model server exited during startup",
        "unknown model architecture: 'flint'",
        "failed to load model",
        "/custom/runtime/bin/llama-server",
        "DIAL_LLAMA_SERVER",
        "LM Studio",
    ] {
        assert!(diagnostic.contains(detail), "missing {detail:?}: {diagnostic}");
    }
    assert!(diagnostic.to_lowercase().contains("upgrad"), "{diagnostic}");
}

#[test]
fn unrelated_startup_failures_preserve_details_without_architecture_advice() {
    let program = std::ffi::OsStr::new("/custom/python");
    for stderr in ["fatal: cannot load model", "out of memory", ""] {
        let error = anyhow!("model server exited during startup");
        let diagnostic = startup_diagnostic(&error, stderr, program);
        assert!(diagnostic.contains("model server exited during startup"));
        assert!(diagnostic.contains("/custom/python"), "{diagnostic}");
        if !stderr.is_empty() {
            assert!(diagnostic.contains(stderr), "{diagnostic}");
        }
        assert!(!diagnostic.contains("DIAL_LLAMA_SERVER"), "{diagnostic}");
        assert!(!diagnostic.contains("LM Studio"), "{diagnostic}");
    }
}

#[test]
fn cancelled_startup_releases_the_server_before_its_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let python = fake_python(dir.path());
    let cancel = Arc::new(AtomicBool::new(false));
    let mut slot = None;
    let started = Instant::now();
    std::thread::scope(|scope| {
        let flag = cancel.clone();
        scope.spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            flag.store(true, Ordering::SeqCst);
        });
        let error = ensure_controlled(&mut slot, &python, "never-ready", None, 30, Some(cancel))
            .err()
            .expect("startup must be cancelled");
        assert!(format!("{error:#}").contains("interrupted"), "{error:#}");
    });
    assert!(slot.is_none());
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn unlimited_startup_can_be_cancelled_during_silent_warmup() {
    let dir = tempfile::tempdir().unwrap();
    let model = dir.path().join("slow-warmup");
    fs::create_dir(&model).unwrap();
    let python = fake_python(dir.path());
    let cancel = Arc::new(AtomicBool::new(false));
    let mut slot = None;
    let started = Instant::now();
    std::thread::scope(|scope| {
        let flag = cancel.clone();
        let marker = model.join("info.json");
        let observer = scope.spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut warmup_started = false;
            while Instant::now() < deadline {
                if let Some(info) = fs::read(&marker)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                    && info["requests"]
                        .as_array()
                        .is_some_and(|requests| !requests.is_empty())
                {
                    warmup_started = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            flag.store(true, Ordering::SeqCst);
            warmup_started
        });
        let error = ensure_controlled(
            &mut slot,
            &python,
            model.to_str().unwrap(),
            None,
            0,
            Some(cancel),
        )
        .err()
        .expect("silent warmup must be cancellable without a startup deadline");
        assert!(
            observer.join().unwrap(),
            "cancellation must occur during warmup"
        );
        assert!(format!("{error:#}").contains("interrupted"), "{error:#}");
    });
    assert!(slot.is_none());
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn startup_timeout_kills_the_child() {
    let dir = tempfile::tempdir().unwrap();
    let python = fake_python(dir.path());
    let started = Instant::now();
    let error = ModelServer::start(&python, "never-ready", None, 1)
        .err()
        .expect("must fail")
        .to_string();
    assert!(error.contains("startup timeout"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn failed_warmup_is_a_start_failure() {
    let dir = tempfile::tempdir().unwrap();
    let python = fake_python(dir.path());
    let error = ModelServer::start(&python, "bad-model", None, 30)
        .err()
        .expect("must fail")
        .to_string();
    assert!(error.contains("warm-up"), "{error}");
    assert!(error.contains("HTTP 500"), "{error}");
}

#[test]
fn ensure_reuses_and_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let python = fake_python(dir.path());
    let first = dir.path().join("a.json");
    let second = dir.path().join("b.json");
    let mut slot = None;
    let url = ensure(&mut slot, &python, first.to_str().unwrap(), None, 30)
        .unwrap()
        .base_url()
        .to_string();
    let pid = info(&first)["pid"].as_i64().unwrap();
    assert_eq!(
        ensure(&mut slot, &python, first.to_str().unwrap(), None, 30)
            .unwrap()
            .base_url(),
        url
    );
    // A different adapter forces a restart and tears the old process down.
    let adapter = dir.path().join("adapter");
    ensure(
        &mut slot,
        &python,
        first.to_str().unwrap(),
        Some(&adapter),
        30,
    )
    .unwrap();
    assert!(wait_dead(pid), "old server not stopped on swap");
    let pid = info(&first)["pid"].as_i64().unwrap();
    // A crashed server is replaced even when the configuration is unchanged.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    for _ in 0..200 {
        if !slot.as_ref().unwrap().is_running() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    ensure(
        &mut slot,
        &python,
        first.to_str().unwrap(),
        Some(&adapter),
        30,
    )
    .unwrap();
    assert!(slot.as_ref().unwrap().is_running());
    assert_ne!(info(&first)["pid"].as_i64().unwrap(), pid);
    ensure(&mut slot, &python, second.to_str().unwrap(), None, 30).unwrap();
    assert!(
        slot.as_ref()
            .unwrap()
            .matches(second.to_str().unwrap(), None)
    );
}

#[test]
fn free_ports_are_loopback_and_usable() {
    // free_port returns a candidate, not a reservation. Other tests or local
    // processes may claim it before bind; startup already retries that race.
    for _ in 0..PORT_ATTEMPTS {
        let port = free_port().unwrap();
        match TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
            Ok(listener) => {
                assert!(listener.local_addr().unwrap().ip().is_loopback());
                return;
            }
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => continue,
            Err(error) => panic!("binding the loopback candidate: {error}"),
        }
    }
    panic!("all loopback candidates were claimed before binding");
}

#[test]
fn stderr_tail_is_bounded() {
    let noise = vec![b'x'; STDERR_KEEP_BYTES * 3];
    let tail = collect_stderr(io::Cursor::new(noise));
    settle_stderr(&tail);
    assert_eq!(
        tail.lock().unwrap_or_else(|p| p.into_inner()).bytes.len(),
        STDERR_KEEP_BYTES
    );
    assert_eq!(tail_text(&tail, ERROR_TAIL_BYTES).len(), ERROR_TAIL_BYTES);
}

// ---- real mlx_lm.server (run with `cargo test --lib server -- --ignored --nocapture`) ----

fn real_python() -> PathBuf {
    std::env::var_os("DIAL_TEST_PYTHON")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../python/.venv/bin/python")
        })
}

fn snapshot(repo_dir: &str) -> String {
    let home = std::env::var("HOME").unwrap();
    let snapshots = Path::new(&home)
        .join(".cache/huggingface/hub")
        .join(repo_dir)
        .join("snapshots");
    let entry = fs::read_dir(&snapshots)
        .unwrap_or_else(|e| panic!("{}: {e}", snapshots.display()))
        .next()
        .expect("no snapshot")
        .unwrap();
    entry.path().to_str().unwrap().to_string()
}

#[test]
#[ignore = "starts the real mlx_lm.server; needs the SmolLM2 snapshot"]
fn real_server_chat() {
    let model = snapshot("models--mlx-community--SmolLM2-135M-Instruct-8bit");
    let server = ModelServer::start(&real_python(), &model, None, 180).unwrap();
    let request = Chat {
        messages: vec![json!({"role": "user", "content": "Say hello in five words."})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 24,
        seed: Some(1),
    };
    let reply = llm::chat(server.base_url(), SERVER_MODEL, &request, 120).unwrap();
    println!("REAL REPLY: {reply:?}");
    assert!(!reply.text.is_empty());
    assert!(reply.completion_tokens.is_some());
}

#[test]
#[ignore = "starts the real mlx_lm.server; set DIAL_TOOL_MODEL_DIR to a tool-capable snapshot"]
fn real_server_tool_calls() {
    let model = std::env::var("DIAL_TOOL_MODEL_DIR").expect("DIAL_TOOL_MODEL_DIR");
    let server = ModelServer::start(&real_python(), &model, None, 180).unwrap();
    let tool = json!({"type": "function", "function": {
        "name": "read_file",
        "description": "Read a text file from the workspace",
        "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}
    }});
    let request = Chat {
        messages: vec![json!({"role": "user", "content": "Please read the file notes.txt"})],
        tools: vec![tool],
        temperature: 0.0,
        max_tokens: 128,
        seed: Some(1),
    };
    let reply = llm::chat(server.base_url(), SERVER_MODEL, &request, 120).unwrap();
    println!("REAL TOOL REPLY: {reply:?}");
    assert!(
        !reply.tool_calls.is_empty(),
        "model produced no parsed tool call"
    );
    assert_eq!(reply.tool_calls[0].name, "read_file");
    assert!(!reply.tool_calls[0].malformed_arguments);
    // Replay the turn with a tool result to prove the history shape is accepted.
    let mut followup = request.clone();
    followup.messages.push(llm::assistant_message(&reply));
    followup.messages.push(llm::tool_message(
        &reply.tool_calls[0].id,
        "hello from notes",
    ));
    let second = llm::chat(server.base_url(), SERVER_MODEL, &followup, 120).unwrap();
    println!("REAL FOLLOWUP: {second:?}");
}
