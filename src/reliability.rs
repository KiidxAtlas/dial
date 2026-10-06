//! Request-scoped tool contracts and measured, model-bound interface selection.
use crate::{
    config::types::Config as AppConfig,
    llm::{self},
    tools,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, time::Instant};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub enabled: bool,
    pub measured_interfaces: bool,
    pub examples: bool,
    pub max_contract_failures: u32,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            measured_interfaces: true,
            examples: true,
            max_contract_failures: 3,
        }
    }
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=8).contains(&self.max_contract_failures),
            "reliability.max_contract_failures must be 1..8"
        );
        Ok(())
    }
}

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
            "[Current executable tool contract]\nOnly these tool names are callable in this request: {}. This snapshot supersedes earlier tool lists. Follow their supplied JSON schemas; never call a tool loaded in this same batch.",
            self.names.join(", ")
        );
        if single {
            out.push_str(" Emit at most one tool call, then wait for its result before choosing the next action.");
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
                    if crate::agent::calls::validate(&schema["function"]["parameters"], &example)
                        .is_ok()
                    {
                        out.push_str(&format!(
                            "\n{name} argument shape (replace example values): {example}"
                        ));
                    }
                }
            }
        }
        out
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
fn binding(config: &AppConfig) -> String {
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
                config.edit_mode,
                include_str!("../python/dial_server.py"),
                config.active_model_profile()
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
pub fn single_call(config: &AppConfig, profile: Option<&Value>, stats: &Stats) -> bool {
    config.reliability.enabled
        && config.reliability.measured_interfaces
        && (config
            .active_model_profile()
            .is_some_and(|p| p.capabilities.parallel_tool_calls == Some(false))
            || stats.consecutive_contract_failures >= 2
            || profile.is_some_and(|p| {
                p["batch_passed"] == false
                    || p["passed"]
                        .as_u64()
                        .zip(p["total"].as_u64())
                        .is_some_and(|(passed, total)| passed < total)
            }))
}
pub fn report(config: &AppConfig, control: &Value) -> Value {
    json!({"enabled":config.reliability.enabled,"measured":measured(config),"session":control["reliability"],"max_contract_failures":config.reliability.max_contract_failures,"strict_tools":config.active_model_profile().is_some_and(|p|p.capabilities.strict_tools),"scope":"fixture tool contracts and execution; not a full coding benchmark"})
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
    let session = &value["session"];
    if session.is_object() {
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
    crate::storage::settings_store::private_dir(&config.state_dir)?;
    let lock = crate::learn::auto::lock_file(&config.state_dir.join("reliability.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock)
        .context("another capability check is already running")?;
    let cancel = crate::repl::install_sigint();
    if config.uses_local_compute() {
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
    if config.uses_local_compute() {
        crate::learn::compute::register(
            &config.user_dir,
            "reliability check",
            &config.project_root,
            &config.state_dir,
            &config.model,
        )?;
    }
    let root = tempfile::tempdir()?;
    let workspace =
        tools::Workspace::new(root.path())?.with_edit_mode(tools::EditMode::from(config.edit_mode));
    let mut backend = crate::agent::make_backend(config)?.without_steering();
    let edit_mode = tools::EditMode::from(config.edit_mode);
    let source = "def value():\n    return 1\n";
    let edit = if edit_mode == tools::EditMode::Hashline {
        Some(
            json!({"input":format!("[fixture.py#{}]\nPUT 2.=2:\n+    return 2",tools::hashline::tag_of(source))}),
        )
    } else {
        Some(json!({"path":"fixture.py","old":"return 1","new":"return 2"}))
    };
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
            crate::agent::todo::schema()
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
            && crate::agent::calls::validate(&contract.schemas[0]["function"]["parameters"], args)
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
        results.push(json!({"tool":name,"passed":passed && executed,"elapsed_ms":started.elapsed().as_millis()}));
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
    crate::storage::settings_store::private_dir(&config.state_dir)?;
    crate::storage::settings_store::write_private(
        &config.state_dir.join("reliability.json"),
        &serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(report)
}

/// Strict schemas encode optional fields as nullable. Unsupported schema compositions stay unmodified.
pub fn strict_parameters(schema: &Value) -> Option<Value> {
    let mut schema = schema.clone();
    if ["oneOf", "anyOf", "allOf", "$ref", "if", "then", "else"]
        .iter()
        .any(|key| schema.get(key).is_some())
    {
        return None;
    }
    match schema["type"].as_str()? {
        "object" => {
            if schema["additionalProperties"] != false {
                return None;
            }
            let required: HashSet<String> = schema["required"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            let properties = schema["properties"].as_object_mut()?;
            let keys: Vec<String> = properties.keys().cloned().collect();
            for (key, child) in properties.iter_mut() {
                *child = strict_parameters(child)?;
                if !required.contains(key) {
                    let kind = child["type"].clone();
                    child["type"] = json!([kind, "null"]);
                    if let Some(values) = child["enum"].as_array_mut() {
                        values.push(Value::Null);
                    }
                }
            }
            schema["required"] = json!(keys);
        }
        "array" => {
            schema["items"] = strict_parameters(&schema["items"])?;
        }
        "string" | "integer" | "number" | "boolean" | "null" => {}
        _ => return None,
    }
    Some(schema)
}
pub fn constrain_openai(body: &mut Value, profile: &llm::profiles::ModelProfile) {
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            if profile.capabilities.strict_tools {
                if let Some(parameters) = strict_parameters(&tool["function"]["parameters"]) {
                    tool["function"]["parameters"] = parameters;
                    tool["function"]["strict"] = json!(true);
                }
            }
        }
        if let Some(parallel) = profile.capabilities.parallel_tool_calls {
            body["parallel_tool_calls"] = json!(parallel);
        }
    }
}
/// Null introduced only for a formerly optional field means omission, not executable data repair.
pub fn omit_optional_nulls(args: &mut Value, schema: &Value) {
    if let Some(object) = args.as_object_mut() {
        let required: HashSet<&str> = schema["required"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let properties = &schema["properties"];
        object.retain(|key, value| {
            !value.is_null()
                || required.contains(key.as_str())
                || properties.get(key).is_none()
                || properties[key]["type"] == "null"
                || properties[key]["type"]
                    .as_array()
                    .is_some_and(|types| types.iter().any(|t| t == "null"))
        });
        for (key, value) in object {
            if let Some(child) = properties.get(key) {
                omit_optional_nulls(value, child);
            }
        }
    } else if let Some(items) = args.as_array_mut() {
        for value in items {
            omit_optional_nulls(value, &schema["items"]);
        }
    }
}
pub fn normalize_reply(reply: &mut llm::Reply, schemas: &[Value]) {
    for call in &mut reply.tool_calls {
        if let Some(schema) = schemas.iter().find(|s| s["function"]["name"] == call.name) {
            if strict_parameters(&schema["function"]["parameters"]).is_some() {
                omit_optional_nulls(&mut call.arguments, &schema["function"]["parameters"]);
            }
        }
    }
}

pub fn live_report(config: &AppConfig, control: &Value) -> Value {
    json!({"enabled":config.reliability.enabled,"measured":control["reliability"]["measured"],"session":control["reliability"],"max_contract_failures":config.reliability.max_contract_failures,"strict_tools":config.active_model_profile().is_some_and(|p|p.capabilities.strict_tools)})
}

/// Keep essential tools and explicitly loaded specialists; discovery can restore every allowed tool.
pub fn simplify(schemas: &mut Vec<Value>, loaded: &HashSet<String>) {
    if !schemas.iter().any(|s| s["function"]["name"] == "tool_load") {
        return;
    }
    schemas.retain(|s| {
        let name = s["function"]["name"].as_str().unwrap_or("");
        loaded.contains(name)
            || tools::BUILTIN.contains(&name)
            || matches!(
                name,
                "todo"
                    | "finish"
                    | "tool_load"
                    | "tool_search"
                    | "codebase_memory"
                    | "artifact_read"
                    | "working_state"
                    | "exit_plan"
            )
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capability_deadline_aborts_incomplete_reply_without_scoring_it() {
        use std::{
            sync::{Arc, atomic::AtomicBool},
            time::Duration,
        };
        let server = crate::llm::fake::serve(vec![crate::llm::fake::Script::Stall {
            silence: Duration::from_millis(250),
        }]);
        let config = AppConfig::default();
        let mut backend = llm::provider::Backend::new(
            llm::provider::ProviderKind::Endpoint {
                base_url: server.url.clone(),
                model: "fixture".into(),
                api_key: None,
            },
            std::path::PathBuf::new(),
            5,
        );
        let chat = llm::Chat {
            messages: vec![json!({"role":"user","content":"fixture"})],
            tools: vec![],
            temperature: 0.,
            max_tokens: 32,
            seed: None,
        };
        let started = Instant::now();
        let error = calibration_chat_with_deadline(
            &config,
            &mut backend,
            &chat,
            &Arc::new(AtomicBool::new(false)),
            Duration::from_millis(20),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("previous measured profile preserved"),
            "{error:#}"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(server.requests().len(), 1);
    }
    #[test]
    fn compact_interface_preserves_loaded_specialists_and_discovery() {
        let mut schemas = vec![
            json!({"function":{"name":"tool_load"}}),
            json!({"function":{"name":"read"}}),
            json!({"function":{"name":"sdk_query"}}),
            json!({"function":{"name":"unused"}}),
        ];
        simplify(&mut schemas, &HashSet::from(["sdk_query".to_owned()]));
        let contract = Contract::new(schemas).unwrap();
        assert_eq!(contract.names, ["tool_load", "read", "sdk_query"]);
    }
    #[test]
    fn request_contract_is_exact_and_examples_validate() {
        let schemas = tools::schemas(&["read", "grep"], tools::EditMode::Replace).unwrap();
        let contract = Contract::new(schemas.clone()).unwrap();
        assert_eq!(contract.names, ["read", "grep"]);
        let text = contract.instruction(true, true);
        assert!(text.contains("at most one tool call"));
        assert!(text.contains("read argument shape"));
        assert!(!text.contains("todo argument shape"));
        assert!(Contract::new(vec![schemas[0].clone(), schemas[0].clone()]).is_err());
    }
    #[test]
    fn strict_optional_nulls_do_not_relax_required_or_unknown_fields() {
        let schema = &tools::schemas(&["read"], tools::EditMode::Replace).unwrap()[0]["function"]["parameters"];
        let strict = strict_parameters(schema).unwrap();
        assert!(
            strict["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|k| k == "limit")
        );
        assert_eq!(
            strict["properties"]["limit"]["type"],
            json!(["integer", "null"])
        );
        let mut args = json!({"path":"fixture.py","limit":null});
        omit_optional_nulls(&mut args, schema);
        assert_eq!(args, json!({"path":"fixture.py"}));
        for mut args in [
            json!({"path":null}),
            json!({"path":"fixture.py","unknown":null}),
            json!({"path":"fixture.py","limit":0}),
        ] {
            omit_optional_nulls(&mut args, schema);
            assert!(crate::agent::calls::validate(schema, &args).is_err());
        }
    }
    #[test]
    fn strict_generation_requires_explicit_support_and_skips_complex_schemas() {
        let mut body = json!({"tools":tools::schemas(&["read"],tools::EditMode::Replace).unwrap()});
        let mut profile = llm::profiles::ModelProfile::default();
        constrain_openai(&mut body, &profile);
        assert!(body["tools"][0]["function"].get("strict").is_none());
        assert!(body.get("parallel_tool_calls").is_none());
        profile.capabilities.strict_tools = true;
        profile.capabilities.parallel_tool_calls = Some(false);
        constrain_openai(&mut body, &profile);
        assert_eq!(body["tools"][0]["function"]["strict"], true);
        assert_eq!(body["parallel_tool_calls"], false);
        assert!(
            strict_parameters(
                &json!({"type":"object","oneOf":[],"additionalProperties":false,"properties":{}})
            )
            .is_none()
        );
    }
    #[test]
    fn recovery_counts_failed_rounds_and_only_progress_resets_them() {
        let config = AppConfig::default();
        let mut stats = Stats::default();
        for _ in 0..2 {
            stats.observe(true, false, false, 12);
            stats.round(true, false);
        }
        assert!(single_call(&config, None, &stats));
        stats.round(false, false);
        assert_eq!(stats.consecutive_contract_failures, 2);
        stats.observe(false, false, true, 25);
        stats.round(false, true);
        assert_eq!(stats.corrections, 1);
        assert_eq!(stats.first_useful_action_ms, Some(25));
        assert_eq!(stats.consecutive_contract_failures, 0);
        assert!(!single_call(&config, None, &stats));
        assert!(single_call(
            &config,
            Some(&json!({"batch_passed":false})),
            &stats
        ));
    }
    #[test]
    fn measurement_is_bound_to_model_backend_and_edit_contract() {
        let root = tempfile::tempdir().unwrap();
        let mut config = AppConfig {
            state_dir: root.path().into(),
            ..Default::default()
        };
        let data = json!({"version":1,"measured_at":crate::learn::improve::now(),"binding":binding(&config),"batch_passed":false});
        std::fs::write(
            root.path().join("reliability.json"),
            serde_json::to_vec(&data).unwrap(),
        )
        .unwrap();
        assert!(measured(&config).is_some());
        config.model.push_str("-changed");
        assert!(measured(&config).is_none());
        config.model = config.model.trim_end_matches("-changed").into();
        config.edit_mode = crate::config::types::EditModeConfig::Hashline;
        assert!(measured(&config).is_none());
    }
    #[test]
    fn busy_report_uses_session_snapshot_without_loading_saved_profile() {
        let config = AppConfig::default();
        let control = json!({"reliability":{"measured":{"passed":4,"total":7},"stats":{"calls":2,"successes":1}}});
        let value = live_report(&config, &control);
        assert_eq!(value["measured"]["passed"], 4);
        assert!(render(&value).contains("1 successful / 2 calls"));
    }
}
