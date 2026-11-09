//! Request-scoped tool contracts and measured, model-bound interface selection.
use crate::config;

use crate::llm::reliability::normalize_reply;
use crate::{
    config::types::{Config as AppConfig, EditMode},
    llm::{self},
    tools::{self},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, time::Instant};

pub struct Contract {
    pub schemas: Vec<Value>,
    pub names: Vec<String>,
}
impl Contract {
    pub fn new(schemas: Vec<Value>) -> Result<Self> {
        let mut seen = HashSet::new();
        let mut names = Vec::new();
        for schema in &schemas {
            let name = schema
                .pointer("/function/name")
                .and_then(Value::as_str)
                .context("tool contract missing name")?;
            ensure!(
                !name.is_empty() && seen.insert(name.to_owned()),
                "duplicate or empty tool contract name"
            );
            names.push(name.to_owned());
        }
        Ok(Self { schemas, names })
    }
    pub fn instruction(&self, single: bool, examples: bool) -> String {
        let mut out = format!(
            "Call only: {}. Follow supplied JSON schemas. Loads apply next request, not this batch.",
            self.names.join(", ")
        );
        if single {
            out.push_str(" Emit at most one tool call; wait for its result.");
        }
        if examples {
            for name in ["read", "grep", "todo"] {
                if self.names.iter().any(|n| n == name) {
                    let example = match name {
                        "read" => json!({"path":"src/example.rs"}),
                        "grep" => json!({"pattern":"symbol","path":"src"}),
                        "todo" => {
                            json!({"op":"append","tasks":[{"content":"Resolve a concrete question","investigation":true}]})
                        }
                        _ => json!({"names":["specialist_name"]}),
                    };
                    // Examples must validate against the exact offered schema, including custom tools.
                    let schema = self
                        .schemas
                        .iter()
                        .find(|s| s["function"]["name"] == name)
                        .unwrap();
                    if crate::agent::run::calls::validate(
                        &schema["function"]["parameters"],
                        &example,
                    )
                    .is_ok()
                    {
                        out.push_str(&format!(
                            "\n{name} argument shape (replace example values): {example}"
                        ));
                    }
                }
            }
        }
        crate::agent::prompts::recovery::notice("tool_contract", &out)
    }
}
#[derive(Default, Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Stats {
    pub calls: u64,
    pub contract_errors: u64,
    pub runtime_errors: u64,
    pub successes: u64,
    pub corrections: u64,
    pub consecutive_contract_failures: u32,
    pub passing_checks: u64,
    pub failing_checks: u64,
    pub first_useful_action_ms: Option<u64>,
}
impl Stats {
    pub fn observe(
        &mut self,
        form_error: bool,
        runtime_error: bool,
        useful: bool,
        elapsed_ms: u64,
    ) {
        self.calls += 1;
        if form_error {
            self.contract_errors += 1;
        } else if runtime_error {
            self.runtime_errors += 1;
        } else {
            self.successes += 1;
            if useful && self.first_useful_action_ms.is_none() {
                self.first_useful_action_ms = Some(elapsed_ms);
            }
        }
    }
    pub fn round(&mut self, contract_failed: bool, useful_progress: bool) {
        if contract_failed {
            self.consecutive_contract_failures += 1;
        } else if useful_progress {
            if self.consecutive_contract_failures > 0 {
                self.corrections += 1;
            }
            self.consecutive_contract_failures = 0;
        }
    }
}
pub(crate) fn binding(config: &AppConfig) -> String {
    // No credentials or prompts are persisted. Local metadata invalidates changed installations.
    let metadata = std::fs::metadata(&config.model).ok().map(|m| {
        (
            m.len(),
            m.modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos()),
        )
    });
    let endpoint = config.endpoint.as_ref().map(|e| &e.base_url);
    let cache = std::env::var_os("HF_HUB_CACHE")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HF_HOME").map(|p| std::path::PathBuf::from(p).join("hub")))
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|p| std::path::PathBuf::from(p).join(".cache/huggingface/hub"))
        });
    let revision = cache.and_then(|cache| {
        std::fs::read_to_string(
            cache
                .join(format!("models--{}", config.model.replace('/', "--")))
                .join("refs/main"),
        )
        .ok()
    });
    format!(
        "{:x}",
        Sha256::digest(
            json!([
                1,
                config.selection(),
                config.providers.get(&config.selection().provider),
                config.model,
                endpoint,
                config.python,
                metadata,
                revision,
                config.edit,
                crate::llm::server::SERVER_SOURCES,
                config.active_model_profile(),
                config.context_window,
                config.assistance.local.sampling
            ])
            .to_string()
        )
    )
}
pub fn measured(config: &AppConfig) -> Option<Value> {
    let bytes = std::fs::read(config.state_dir.join("reliability.json")).ok()?;
    if bytes.len() > 65536 {
        return None;
    }
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    (value["binding"] == binding(config)
        && value["version"] == 1
        && value["measured_at"].as_u64().is_some_and(|at| {
            let now = crate::learn::improve::now();
            at <= now && now - at <= 7 * 24 * 60 * 60
        }))
    .then_some(value)
}
/// Whether a saved record shows the two-call batch fixture passing in full. The prompt's one-call
/// rule and the offered contract both read this, so they cannot disagree.
pub fn batch_permitted(record: &Value) -> bool {
    record["batch_passed"] == true
        && record["passed"]
            .as_u64()
            .zip(record["total"].as_u64())
            .is_none_or(|(passed, total)| passed >= total)
}
/// Whether a saved record has a fixture case (other than the batch case) that did not pass natively:
/// it failed, or Dial had to lift its calls out of text, which is not the server's own parsing.
pub fn native_failure(record: &Value) -> bool {
    record["cases"].as_array().is_some_and(|cases| {
        cases
            .iter()
            .any(|c| c["tool"] != "batch" && (c["passed"] == false || c["text_lifted"] == true))
    })
}
/// Why measured reliability constrains this request. Order is stable for diagnostics.
pub fn single_call_reasons(
    config: &AppConfig,
    profile: Option<&Value>,
    stats: &Stats,
) -> Vec<&'static str> {
    if !config.reliability.enabled || !config.reliability.measured_interfaces {
        return Vec::new();
    }
    let mut reasons = Vec::new();
    if config
        .active_model_profile()
        .is_some_and(|p| p.capabilities.parallel_tool_calls == Some(false))
    {
        reasons.push("model_capability");
    }
    if stats.consecutive_contract_failures >= 2 {
        reasons.push("contract_recovery");
    }
    if profile.is_some_and(|p| !batch_permitted(p)) {
        reasons.push("saved_reliability_record");
    }
    reasons
}

pub fn single_call(config: &AppConfig, profile: Option<&Value>, stats: &Stats) -> bool {
    !single_call_reasons(config, profile, stats).is_empty()
}
/// Route facts for the CLI report (`dial reliability`). Only this path may probe a local server or
/// read the saved record; the live session never calls it.
pub fn route(config: &AppConfig) -> Value {
    let record = measured(config);
    let selected = record
        .as_ref()
        .map(|r| interface_name(single_call(config, Some(r), &Stats::default())));
    let source = if selected.is_some() {
        "measured record (CLI, default stats)"
    } else {
        "unmeasured"
    };
    let server = config.local_main_model().then(|| probe_server(config));
    route_json(config, None, server, record.as_ref(), selected, source)
}
/// Route facts for the turn being sent. The caller passes the record it already read for this turn
/// and the interface it selected, so no disk read, server probe or model call happens here.
pub fn route_snapshot(
    config: &AppConfig,
    effective_window: u32,
    record: Option<&Value>,
    one_call: bool,
) -> Value {
    route_json(
        config,
        Some(effective_window),
        None,
        record,
        Some(interface_name(one_call)),
        "this session",
    )
}
fn interface_name(one_call: bool) -> &'static str {
    if one_call { "one_call" } else { "batch" }
}
/// The local server's own facts. Asked only by the CLI route report.
fn probe_server(config: &AppConfig) -> Value {
    let selection = config.selection();
    let base = config.endpoint.as_ref().map(|e| e.base_url.as_str());
    let token = config.endpoint.as_ref().and_then(|e| {
        crate::llm::catalog::resolve_credential(
            &config.user_dir,
            &e.base_url,
            e.api_key_env.as_deref(),
        )
        .ok()
        .flatten()
    });
    let headers = config.request_headers();
    let facts = crate::llm::local_server::facts(
        &selection.provider,
        &selection.model,
        base,
        token.as_deref(),
        headers,
    );
    let kind = crate::llm::local_hints::LocalServer::detect(
        &selection.provider,
        &selection.model,
        base,
        token.as_deref(),
        headers,
    );
    json!({"kind":format!("{kind:?}"),"slots":facts.slots,"native_tools":facts.native_tools,"thinking_switch":facts.thinking_switch})
}
fn route_json(
    config: &AppConfig,
    effective: Option<u32>,
    server: Option<Value>,
    record: Option<&Value>,
    selected: Option<&str>,
    source: &str,
) -> Value {
    let selection = config.selection();
    let profile = config.active_model_profile();
    let local = config.local_main_model();
    let base = config.endpoint.as_ref().map(|e| e.base_url.as_str());
    json!({
        "provider": selection.provider,
        "model": selection.model,
        "endpoint": base.map(crate::llm::display_url),
        "local": local,
        // `effective` is the window this session plans with; null when only the config is known.
        "context_window": {"configured": config.context_window, "effective": effective},
        "sampling": local.then_some(config.assistance.local.sampling),
        "server_probed": server.is_some(),
        "declared": {
            "strict_tools": profile.is_some_and(|p| p.capabilities.strict_tools),
            "parallel_tool_calls": profile.and_then(|p| p.capabilities.parallel_tool_calls),
        },
        "server": server,
        "measured": record.map(|r| json!({"batch_passed":r["batch_passed"],"passed":r["passed"],"total":r["total"],"cases":r["cases"]})),
        // Null when no fixture record exists and no session selection is known; never a guess.
        "selected_interface": selected,
        "selected_interface_source": source,
    })
}
pub fn report(config: &AppConfig, control: &Value) -> Value {
    json!({"enabled":config.reliability.enabled,"measured":measured(config),"route":route(config),"session":control["reliability"],"max_contract_failures":config.reliability.max_contract_failures,"strict_tools":config.active_model_profile().is_some_and(|p|p.capabilities.strict_tools),"scope":"fixture tool contracts and execution; not a full coding benchmark"})
}
pub fn render(value: &Value) -> String {
    let mut out = format!(
        "Reliability {} · constraints {}\nCapability profile: {}\nRun dial reliability check to measure the selected backend; /reliability shows live turn results.",
        if value["enabled"] == true {
            "enabled"
        } else {
            "disabled"
        },
        if value["strict_tools"] == true {
            "declared strict tools"
        } else {
            "post-generation validation; decoding constraints undeclared"
        },
        if value["measured"].is_object() {
            "current measured profile"
        } else {
            "unmeasured or stale"
        }
    );
    if value["measured"].is_object() {
        out.push_str(&format!(
            "\nFixture cases: {}/{} · batch passed: {}",
            value["measured"]["passed"],
            value["measured"]["total"],
            if value["measured"]["batch_disabled"] == true {
                "disabled by profile".to_owned()
            } else {
                value["measured"]["batch_passed"].to_string()
            }
        ));
    }
    if value["route"].is_object() {
        let route = &value["route"];
        let fact = |v: &Value| v.as_bool().map_or("unknown".to_owned(), |b| b.to_string());
        out.push_str(&format!(
            "\nRoute: {}/{} · endpoint {} · context configured {} tokens, effective {} · sampling {}",
            route["provider"].as_str().unwrap_or("?"),
            route["model"].as_str().unwrap_or("?"),
            route["endpoint"].as_str().unwrap_or("none (Dial runtime)"),
            route["context_window"]["configured"],
            route["context_window"]["effective"].as_u64().map_or("not probed".to_owned(), |t| format!("{t} tokens")),
            route["sampling"].as_str().map_or("not local".to_owned(), |s| format!("{s} (policy, not measured)")),
        ));
        out.push_str(&format!(
            "\nServer: {} · declared strict tools {} · declared parallel calls {} · server native tools {} · server thinking switch {}\nSelected interface: {}",
            route["server"]["kind"].as_str().unwrap_or("not asked (hosted or unavailable)"),
            fact(&route["declared"]["strict_tools"]),
            fact(&route["declared"]["parallel_tool_calls"]),
            fact(&route["server"]["native_tools"]),
            fact(&route["server"]["thinking_switch"]),
            route["selected_interface"].as_str().map_or("unmeasured (no fixture record for this backend)".to_owned(), |s| format!("{s} · {}", route["selected_interface_source"].as_str().unwrap_or("?"))),
        ));
    }
    let session = &value["session"];
    if session.is_object() {
        if session["call_policy"].is_object() {
            out.push_str(&format!(
                "\nRequest call policy: {} · reasons: {}",
                session["call_policy"]["mode"].as_str().unwrap_or("unknown"),
                session["call_policy"]["reasons"]
                    .as_array()
                    .map(|reasons| reasons
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", "))
                    .filter(|reasons| !reasons.is_empty())
                    .unwrap_or_else(|| "no one-call constraint".into()),
            ));
        }
        let s = &session["stats"];
        let first_action = s["first_useful_action_ms"]
            .as_u64()
            .map(|ms| format!("{ms} ms"))
            .unwrap_or_else(|| "not yet recorded".into());
        let rate = s["calls"]
            .as_u64()
            .filter(|calls| *calls > 0)
            .map(|calls| {
                format!(
                    "{:.1}%",
                    100.0 * s["successes"].as_u64().unwrap_or(0) as f64 / calls as f64
                )
            })
            .unwrap_or_else(|| "no calls yet".into());
        out.push_str(&format!("\nTool success rate: {rate}"));
        out.push_str(&format!("\nTurn: {} successful / {} calls · {} contract errors · {} runtime errors · {} corrections\nChecks: {} passed / {} failed · first useful action: {}\nInterface: {} · verification: {}",s["successes"],s["calls"],s["contract_errors"],s["runtime_errors"],s["corrections"],json!(s["passing_checks"].as_u64().unwrap_or(0)+session["automatic_passing_checks"].as_u64().unwrap_or(0)),json!(s["failing_checks"].as_u64().unwrap_or(0)+session["automatic_failing_checks"].as_u64().unwrap_or(0)),first_action,session["interface"].as_str().unwrap_or("standard"),session["verification"].as_str().unwrap_or("not recorded")));
        if session["edit_loop"].is_object() {
            out.push_str(&format!(
                "\nEdit loop: {} · {} edits · {} checked · {} diffs surfaced · {} shell commands ({} disclosed) · {} extension calls ({} disclosed)",
                session["edit_loop"]["phase"].as_str().unwrap_or("unknown"),
                session["edit_loop"]["mutations"].as_u64().unwrap_or(0),
                session["edit_loop"]["checked_mutations"].as_u64().unwrap_or(0),
                session["edit_loop"]["diff_surfaced_mutations"].as_u64().unwrap_or(0),
                session["edit_loop"]["uncheckpointed_shell_commands"].as_u64().unwrap_or(0),
                session["edit_loop"]["disclosed_shell_commands"].as_u64().unwrap_or(0),
                session["edit_loop"]["uncheckpointed_extension_calls"].as_u64().unwrap_or(0),
                session["edit_loop"]["disclosed_extension_calls"].as_u64().unwrap_or(0),
            ));
            if let Some(failed) = session["edit_loop"]["failed_checks"].as_array()
                && !failed.is_empty()
            {
                let names = failed
                    .iter()
                    .filter_map(|name| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                out.push_str(&format!("\nFailed focused checks: {names}"));
            }
            if let Some(tools) = session["edit_loop"]["uncheckpointed_extension_tools"].as_array()
                && !tools.is_empty()
            {
                let names = tools
                    .iter()
                    .filter_map(|tool| tool.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                out.push_str(&format!("\nUncheckpointed extension tools: {names}"));
            }
        }
    }
    if let Some(state) = value["auto"]["state"].as_str() {
        out.push_str(&format!("\nAuto check: {state}"));
    }
    out
}
/// A wall-clock bound, including retries and active streams, independent of idle timeouts.
fn calibration_chat(
    config: &AppConfig,
    backend: &mut llm::provider::Backend,
    chat: &llm::Chat,
    external: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<llm::Reply> {
    calibration_chat_with_deadline(
        config,
        backend,
        chat,
        external,
        std::time::Duration::from_secs(60),
    )
}
fn calibration_chat_with_deadline(
    config: &AppConfig,
    backend: &mut llm::provider::Backend,
    chat: &llm::Chat,
    external: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    budget: std::time::Duration,
) -> Result<llm::Reply> {
    crate::learn::compute::background_call_controlled(
        config,
        backend,
        Some(external),
        |backend, interrupt| {
            let timed_out = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let reply = std::thread::scope(|scope| {
                let (done, rx) = std::sync::mpsc::channel::<()>();
                let deadline = timed_out.clone();
                let flag = interrupt.clone();
                scope.spawn(move || {
                    if matches!(
                        rx.recv_timeout(budget),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                    ) {
                        deadline.store(true, std::sync::atomic::Ordering::SeqCst);
                        flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                });
                let result = backend.chat_with_control(
                    None,
                    chat,
                    config.worker_timeout_secs.min(60),
                    None,
                    &mut |_| {},
                    interrupt,
                );
                drop(done);
                result
            });
            ensure!(
                !timed_out.load(std::sync::atomic::Ordering::SeqCst),
                "capability case exceeded its {:?} time budget; previous measured profile preserved",
                budget
            );
            reply
        },
    )
}

/// Explicit bounded calibration. Model calls can only touch disposable fixtures after exact checks.
pub fn check(config: &AppConfig) -> Result<Value> {
    crate::platform::atomic::private_dir(&config.state_dir)?;
    let lock = crate::learn::auto::lock_file(&config.state_dir.join("reliability.lock"))?;
    std::fs::File::try_lock(&lock)
        .map_err(std::io::Error::from)
        .context("another capability check is already running")?;
    let cancel = crate::platform::interrupt::install_sigint();
    if config.requires_compute_lock() {
        eprintln!(
            "Capability check queued; waiting for idle compute. Foreground coding preempts; Ctrl+C stops."
        );
    }
    let _lease = if config.uses_local_compute() {
        Some(crate::learn::compute::Background::acquire_controlled(
            config,
            Some(&cancel),
        )?)
    } else {
        None
    };
    if config.requires_compute_lock() {
        crate::learn::compute::register(
            &config.user_dir,
            "reliability check",
            &config.project_root,
            &config.state_dir,
            &config.model,
        )?;
    }
    let root = tempfile::tempdir()?;
    let edit_mode = config.resolved_edit_mode(&config.harness);
    let workspace = tools::Workspace::new(root.path())?
        .with_edit_settings(config.edit.clone())
        .with_ssh_config(config.ssh.clone())
        .with_ssh_hosts(&config.ssh_hosts)
        .with_edit_mode(edit_mode);
    let mut backend = crate::agent::make_backend(config)?.without_steering();
    let source = "def value():\n    return 1\n";
    let edit = Some(match edit_mode {
        EditMode::Hashline => {
            json!({"input":format!("[fixture.py#{}]\nPUT 2.=2:\n+    return 2",crate::tools::edit::hashline::tag_of(source))})
        }
        EditMode::Replace => {
            json!({"path":"fixture.py","old_string":"return 1","new_string":"return 2"})
        }
        EditMode::ApplyPatch => {
            json!({"input":"*** Begin Patch\n*** Update File: fixture.py\n@@\n def value():\n-    return 1\n+    return 2\n*** End Patch"})
        }
        EditMode::Patch => {
            json!({"path":"fixture.py","edits":[{"op":"update","diff":"@@\n def value():\n-    return 1\n+    return 2"}]})
        }
        EditMode::Sloppy => {
            json!({"input":"*** Edit File: fixture.py\n*** Find\n    return 1\n*** Replace\n    return 2"})
        }
    });
    let mut cases = vec![
        ("read", json!({"path":"fixture.py"})),
        ("grep", json!({"pattern":"return","path":"fixture.py"})),
        ("glob", json!({"pattern":"*.py"})),
        (
            "write",
            json!({"path":"created.txt","content":"verified fixture"}),
        ),
        (
            "todo",
            json!({"op":"set","tasks":[{"content":"Inspect fixture","investigation":true}]}),
        ),
    ];
    if let Some(edit) = edit {
        cases.push(("edit", edit));
    }
    cases.push((
        "bash",
        json!({"command":"test -f created.txt && test -s fixture.py"}),
    ));
    let mut results = Vec::new();
    for (name, args) in &cases {
        std::fs::write(
            root.path().join("fixture.py"),
            "def value():\n    return 1\n",
        )?;
        if *name == "edit" {
            ensure!(
                !tools::execute(&workspace, "read", &json!({"path":"fixture.py"}), 10).is_error,
                "fixture read failed"
            );
        }
        let schema = if *name == "todo" {
            crate::agent::workflow::todo::schema()
        } else {
            tools::schemas(&[name], edit_mode)?.remove(0)
        };
        let contract = Contract::new(vec![schema])?;
        let chat = llm::Chat {
            messages: vec![
                json!({"role":"system","content":"Use the supplied tool exactly as requested; no prose. This is a disposable capability fixture."}),
                json!({"role":"user","content":format!("Call {name} exactly once with these arguments: {args}")}),
            ],
            tools: contract.schemas.clone(),
            temperature: 0.,
            max_tokens: 512,
            seed: None,
        };
        eprintln!(
            "Checking {name} ({}/{})",
            results.len() + 1,
            cases.len() + 1
        );
        let started = Instant::now();
        let result = calibration_chat(config, &mut backend, &chat, &cancel);
        if cancel.load(std::sync::atomic::Ordering::SeqCst) {
            anyhow::bail!("reliability check interrupted; previous profile preserved");
        }
        // Transport failures invalidate the run, not the model's tool capability.
        let mut reply = result?;
        // Reply-level marker: tool calls Dial lifted out of text are not native server parsing.
        let lifted = reply.text_calls_lifted;
        if config
            .active_model_profile()
            .is_some_and(|p| p.capabilities.strict_tools)
        {
            normalize_reply(&mut reply, &chat.tools);
        }
        let passed = reply.tool_calls.len() == 1
            && reply.tool_calls[0].name == *name
            && !reply.tool_calls[0].malformed_arguments
            && reply.tool_calls[0].arguments == *args
            && crate::agent::run::calls::validate(
                &contract.schemas[0]["function"]["parameters"],
                args,
            )
            .is_ok();
        let executed = if passed && *name != "todo" {
            let out = tools::execute(&workspace, name, args, 10);
            ensure!(
                !out.is_error,
                "capability fixture execution failed for {name}: {}. Previous measured profile preserved",
                out.text
            );
            match *name {
                "write" => {
                    std::fs::read_to_string(root.path().join("created.txt"))
                        .ok()
                        .as_deref()
                        == Some("verified fixture")
                }
                "edit" => std::fs::read_to_string(root.path().join("fixture.py"))
                    .ok()
                    .is_some_and(|s| s.contains("return 2")),
                _ => true,
            }
        } else {
            passed
        };
        results.push(json!({"tool":name,"passed":passed && executed,"native":passed && executed && !lifted,"text_lifted":lifted,"elapsed_ms":started.elapsed().as_millis()}));
        eprintln!(
            "Capability check {}/{}: {name} {}",
            results.len(),
            cases.len() + 1,
            if passed && executed {
                "passed"
            } else {
                "failed"
            }
        );
    }
    let batch_disabled = config
        .active_model_profile()
        .is_some_and(|p| p.capabilities.parallel_tool_calls == Some(false));
    let batch_passed = if batch_disabled {
        eprintln!("Batch check skipped: disabled by the selected model profile");
        false
    } else {
        eprintln!(
            "Checking two-call batch ({}/{})",
            cases.len() + 1,
            cases.len() + 1
        );
        let schemas = tools::schemas(&["read", "glob"], edit_mode)?;
        let chat = llm::Chat {
            messages: vec![
                json!({"role":"user","content":"In one batch call read with {\"path\":\"fixture.py\"} and glob with {\"pattern\":\"*.py\"}. No other calls."}),
            ],
            tools: schemas,
            temperature: 0.,
            max_tokens: 512,
            seed: None,
        };
        let mut reply = calibration_chat(config, &mut backend, &chat, &cancel)?;
        ensure!(
            !cancel.load(std::sync::atomic::Ordering::SeqCst),
            "reliability check interrupted; previous profile preserved"
        );
        if config
            .active_model_profile()
            .is_some_and(|p| p.capabilities.strict_tools)
        {
            normalize_reply(&mut reply, &chat.tools);
        }
        reply.tool_calls.len() == 2
            && [
                ("read", json!({"path":"fixture.py"})),
                ("glob", json!({"pattern":"*.py"})),
            ]
            .iter()
            .all(|(name, args)| {
                reply
                    .tool_calls
                    .iter()
                    .any(|c| c.name == *name && c.arguments == *args && !c.malformed_arguments)
            })
    };
    results.push(json!({"tool":"batch","passed":batch_passed,"skipped":batch_disabled}));
    let passed = results.iter().filter(|r| r["passed"] == true).count();
    let report = json!({"version":1,"measured_at":crate::learn::improve::now(),"binding":binding(config),"model":config.model,"passed":passed,"total":results.len()-usize::from(batch_disabled),"batch_disabled":batch_disabled,"batch_passed":batch_passed,"cases":results,"scope":"base model without LoRA or steering; disposable fixture contracts; no full coding accuracy certification"});
    crate::platform::atomic::private_dir(&config.state_dir)?;
    crate::config::settings_store::write_private(
        &config.state_dir.join("reliability.json"),
        &serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(report)
}

pub fn live_report(config: &AppConfig, control: &Value) -> Value {
    json!({"enabled":config.reliability.enabled,"measured":control["reliability"]["measured"],"route":control["reliability"]["route"],"session":control["reliability"],"max_contract_failures":config.reliability.max_contract_failures,"strict_tools":config.active_model_profile().is_some_and(|p|p.capabilities.strict_tools)})
}

/// Keep essential tools and explicitly loaded specialists; discovery can restore every allowed tool.
pub fn simplify(schemas: &mut Vec<Value>, loaded: &HashSet<String>) {
    if !schemas.iter().any(|s| s["function"]["name"] == "tool_load") {
        return;
    }
    schemas.retain(|s| {
        let name = s["function"]["name"].as_str().unwrap_or("");
        loaded.contains(name)
            || config::tools::BUILTIN.contains(&name)
            || matches!(
                name,
                "todo"
                    | "finish"
                    | "tool_load"
                    | "tool_search"
                    | "codebase_memory"
                    | "context_notes"
                    | "new_context"
                    | "exit_plan"
            )
    });
}

#[cfg(test)]
mod tests;
