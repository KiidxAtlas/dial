//! Bounded assistance from observed work. No training labels or admission decisions live here.
use super::*;
use crate::storage::experience::{self, Recipe, Step};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct State {
    observations: Vec<Value>,
    hypotheses: Vec<Value>,
    #[serde(skip)]
    steps: Vec<Step>,
    #[serde(skip)]
    checks: Vec<String>,
    #[serde(skip)]
    objective: String,
    #[serde(skip)]
    eligible: bool,
    #[serde(skip)]
    mutated: bool,
    #[serde(skip)]
    pub scores: BTreeMap<String, f64>,
    #[serde(skip)]
    recalled: Vec<(String, Vec<String>)>,
    #[serde(skip)]
    reused: HashSet<String>,
}
impl State {
    pub fn restore(control: &Value) -> Self {
        let mut state: Self =
            serde_json::from_value(control["investigation_ledger"].clone()).unwrap_or_default();
        state.observations.truncate(64);
        state.hypotheses.truncate(16);
        state
    }
    pub fn begin(&mut self, objective: &str) {
        self.steps.clear();
        self.checks.clear();
        self.objective = prompt::truncate_chars(objective, 1024);
        self.eligible = true;
        self.mutated = false;
        self.recalled.clear();
        self.reused.clear();
    }
    pub fn check(&mut self, command: &str, passed: bool) {
        if passed && command.len() <= 2048 && !self.checks.iter().any(|c| c == command) {
            self.checks.push(command.into());
            self.checks.truncate(8);
        } else if !passed {
            self.checks.clear();
        }
    }
}
pub(super) fn schema() -> Value {
    json!({"type":"function","function":{"name":"experience","description":"Inspect project experience and investigation observations; record a hypothesis with an observed evidence ID, or replay a source-matched verified recipe through normal approvals. Memories are advisory, never permission grants.","parameters":{"type":"object","properties":{"action":{"type":"string","enum":["inspect","ledger","hypothesis","run"]},"id":{"type":"string"},"hypothesis":{"type":"string","maxLength":1024},"status":{"type":"string","enum":["open","supported","ruled_out"]}},"required":["action"],"additionalProperties":false}}})
}
/// Rank only the caller's eligible catalog; discovery cannot widen that boundary.
pub(super) fn discover(
    query: &str,
    mut candidates: Vec<(String, String)>,
) -> Vec<(String, String, bool)> {
    let query = query.trim();
    if let Some(index) = candidates
        .iter()
        .position(|(name, _)| name.eq_ignore_ascii_case(query))
    {
        let (name, description) = candidates.swap_remove(index);
        return vec![(name, description, true)];
    }
    let words = |text: &str| -> HashSet<String> {
        text.split(|c: char| !c.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .map(str::to_lowercase)
            .collect()
    };
    // Remove grammatical filler, not domain terms such as "read", "web" or "file".
    let stopwords = [
        "a",
        "an",
        "the",
        "and",
        "or",
        "to",
        "for",
        "of",
        "in",
        "on",
        "at",
        "by",
        "with",
        "from",
        "that",
        "this",
        "these",
        "those",
        "is",
        "are",
        "be",
        "can",
        "could",
        "would",
        "should",
        "i",
        "me",
        "my",
        "we",
        "our",
        "you",
        "your",
        "please",
        "need",
        "want",
        "use",
        "using",
        "tool",
        "tools",
        "capability",
    ];
    let mut terms = words(query);
    terms.retain(|term| !stopwords.contains(&term.as_str()));
    if terms.is_empty() {
        return Vec::new();
    }
    let indexed: Vec<_> = candidates
        .into_iter()
        .map(|(name, description)| {
            let name_words = words(&name);
            let mut all_words = words(&description);
            all_words.extend(name_words.iter().cloned());
            (name, description, name_words, all_words)
        })
        .collect();
    // Unknown query terms carry no evidence. Rare terms discriminate capabilities
    // better than boilerplate shared by many descriptions.
    let weights: BTreeMap<_, _> = terms
        .into_iter()
        .filter_map(|term| {
            let count = indexed
                .iter()
                .filter(|entry| entry.3.contains(&term))
                .count();
            (count > 0).then(|| (term, (indexed.len() + 1) as f64 / (count + 1) as f64))
        })
        .collect();
    let total: f64 = weights.values().sum();
    if total == 0.0 {
        return Vec::new();
    }
    let mut ranked: Vec<_> = indexed
        .into_iter()
        .filter_map(|(name, description, name_words, all_words)| {
            let matched: f64 = weights
                .iter()
                .filter(|(term, _)| all_words.contains(*term))
                .map(|(_, weight)| weight)
                .sum();
            let name_score: f64 = weights
                .iter()
                .filter(|(term, _)| name_words.contains(*term))
                .map(|(_, weight)| weight)
                .sum();
            // A lone shared word must not activate unrelated capabilities when
            // the query contains stronger discriminating evidence.
            (matched >= total * 0.5).then_some((name, description, matched + name_score * 2.0))
        })
        .collect();
    ranked.sort_by(|a, b| b.2.total_cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    let best = ranked.first().map_or(0.0, |entry| entry.2);
    ranked
        .into_iter()
        .take(8)
        .enumerate()
        .map(|(index, (name, description, score))| {
            (name, description, index < 3 && score >= best * 0.85)
        })
        .collect()
}

impl Agent<'_> {
    pub(super) fn plan_tool_available(&self, name: &str) -> bool {
        !self.plan_mode
            || matches!(
                name,
                "task" | "bg" | "lsp" | "ast_edit" | "working_state" | "tool_search" | "tool_load"
            )
            || approval::is_read_only(&self.ext, name, &json!({}))
    }
}

pub(super) fn compact(
    names: &mut Vec<String>,
    loaded: &HashSet<String>,
    limit: usize,
    scores: &BTreeMap<String, f64>,
) {
    if limit == 0 {
        names.sort();
        return;
    }
    let core = [
        "read",
        "write",
        "edit",
        "bash",
        "grep",
        "find",
        "glob",
        "todo",
        "finish",
        "tool_search",
        "tool_load",
        "artifact_read",
        "working_state",
        "codebase_memory",
        "task",
        "skill",
    ];
    let required_tool = |name: &String| loaded.contains(name) || core.contains(&name.as_str());
    names.sort_by(|a, b| {
        let core_a = required_tool(a);
        let core_b = required_tool(b);
        core_b
            .cmp(&core_a)
            .then_with(|| {
                scores
                    .get(b)
                    .copied()
                    .unwrap_or(0.5)
                    .total_cmp(&scores.get(a).copied().unwrap_or(0.5))
            })
            .then_with(|| a.cmp(b))
    });
    let required = names.iter().filter(|n| required_tool(n)).count();
    names.truncate(limit.max(required));
    names.sort();
}
impl Agent<'_> {
    pub(super) fn experience_sources(
        &self,
        call: &ToolCall,
        ui: &mut dyn Ui,
    ) -> BTreeMap<String, Option<String>> {
        if self.experience.is_none() {
            return BTreeMap::new();
        }
        let ws = self.workspace.clone();
        let call = call.clone();
        thread::scope(|scope| {
            let (tx, rx) = mpsc::sync_channel(1);
            scope.spawn(move || {
                let _ = tx.send(experience::preconditions(&ws, &call.name, &call.arguments));
            });
            receive_with_heartbeat(&rx, ui).unwrap_or_default()
        })
    }
    pub(super) fn experience_context(&mut self, query: &str, ui: &mut dyn Ui) -> String {
        let Some(store) = self.experience.clone() else {
            if self.config.improve.enabled && self.config.assistance.enabled && !self.hermetic {
                ui.diagnostic("Project experience is unavailable; coding remains available. /experience shows storage diagnostics.");
            }
            return String::new();
        };
        let ws = self.workspace.clone();
        let query = prompt::truncate_chars(query, 1024);
        let model = self.config.model.clone();
        let budget = super::efficiency::byte_limit(self.config.assistance.context_max_bytes).min(
            if self.config.efficiency.enabled {
                super::efficiency::byte_limit(self.config.efficiency.ambient_context_bytes)
            } else {
                usize::MAX
            },
        );
        let result = thread::scope(|scope| {
            let (tx, rx) = mpsc::sync_channel(1);
            scope.spawn(move || {
                let result = (|| -> Result<_> {
                    let recalled = store.recall(&query, &model, &ws, budget)?;
                    let scores = store.tool_scores(&model)?;
                    let records = recalled
                        .1
                        .iter()
                        .filter_map(|id| {
                            store.get_recipe(id).ok().map(|r| {
                                (
                                    id.clone(),
                                    r.steps
                                        .into_iter()
                                        .flat_map(|s| s.sources.into_keys())
                                        .collect::<Vec<_>>(),
                                )
                            })
                        })
                        .collect::<Vec<_>>();
                    Ok((recalled.0, scores, records))
                })();
                let _ = tx.send(result);
            });
            receive_with_heartbeat(&rx, ui).ok().and_then(Result::ok)
        });
        let (recalled, scores, records) = result.unwrap_or_default();
        self.assistance.scores = scores;
        self.assistance.recalled = records;
        // Only unchanged evidence can prevent a read. The ledger is informational; the ordinary read tool remains available.
        let observations = self
            .assistance
            .observations
            .iter()
            .rev()
            .take(4)
            .cloned()
            .collect::<Vec<_>>();
        let ws = self.workspace.clone();
        let ledger_budget = budget.saturating_sub(recalled.len()).min(1024);
        let ledger = thread::scope(|scope| {
            let (tx, rx) = mpsc::sync_channel(1);
            scope.spawn(move||{
            let mut ledger=String::new();
            for entry in observations {let sources=serde_json::from_value(entry["sources"].clone()).unwrap_or_default();
                if experience::applicable(&ws,&sources) {let line=format!("Previously observed {} via {} for {}; unchanged source evidence. Output digest {}, reread if full content is needed.\n",entry["id"],entry["tool"],entry["question"],entry["digest"]);if ledger.len()+line.len()<=ledger_budget{ledger.push_str(&line);}}}
            let _=tx.send(ledger);
        });
            receive_with_heartbeat(&rx, ui).unwrap_or_default()
        });
        if recalled.is_empty() && ledger.is_empty() {
            String::new()
        } else {
            format!(
                "\nProject experience (advisory; source-scoped, observations are not behavioral proof):\n{recalled}{ledger}\n"
            )
        }
    }
    pub(super) fn observe_experience(
        &mut self,
        call: &ToolCall,
        out: &ToolOutput,
        sources: BTreeMap<String, Option<String>>,
        ui: &mut dyn Ui,
    ) {
        let Some(store) = self.experience.clone() else {
            return;
        };
        let model = self.config.model.clone();
        let tool = call.name.clone();
        let args = call.arguments.clone();
        let conditions = sources.clone();
        let kind = experience::failure_kind(&out.text);
        let ok = !out.is_error;
        thread::scope(|scope| {
            let (tx, rx) = mpsc::sync_channel(1);
            scope.spawn(move || {
                let _ = store.observe_tool(&model, &tool, ok);
                if !ok {
                    let _ = store.failure(&model, &tool, &args, &conditions, kind);
                }
                let _ = tx.send(());
            });
            let _ = receive_with_heartbeat(&rx, ui);
        });
        if out.is_error {
            return;
        }
        if matches!(call.name.as_str(), "read" | "grep" | "glob" | "ast_search")
            && !sources.is_empty()
        {
            let digest = format!("{:x}", Sha256::digest(out.text.as_bytes()));
            let id = format!(
                "{:x}",
                Sha256::digest(format!("{}:{}:{sources:?}", call.name, call.arguments).as_bytes())
            );
            if !self.assistance.observations.iter().any(|v| v["id"] == id) {
                self.assistance.observations.push(json!({"id":id,"tool":call.name,"question":self.todo.tasks().iter().find(|task|task.investigation&&task.status.is_open()).map(|task|task.content.as_str()).unwrap_or(&self.assistance.objective),"sources":sources,"digest":digest}));
                if self.assistance.observations.len() > 64 {
                    self.assistance.observations.remove(0);
                }
            }
        }
        let touched = experience::paths(&call.name, &call.arguments);
        for (id, paths) in &self.assistance.recalled {
            if touched.iter().any(|path| paths.contains(path)) {
                self.assistance.reused.insert(id.clone());
            }
        }
        let mut captured = 0usize;
        if matches!(call.name.as_str(), "write" | "edit" | "ast_edit") {
            self.assistance.mutated = true;
            self.assistance.checks.clear();
            for (path, hash) in &sources {
                if let Ok(target) = self.workspace.resolve(path)
                    && let Ok(content) = std::fs::read_to_string(target)
                    && content.len() <= 1024 * 1024
                {
                    self.assistance.steps.push(Step {
                        tool: "write".into(),
                        args: json!({"path":path,"content":content}),
                        sources: BTreeMap::from([(path.clone(), hash.clone())]),
                    });
                    captured += 1;
                }
            }
            if captured == 0 || captured != sources.len() {
                self.assistance.eligible = false;
            }
        } else if matches!(call.name.as_str(), "read" | "grep" | "glob" | "ast_search")
            && !sources.is_empty()
        {
            self.assistance.steps.push(Step {
                tool: call.name.clone(),
                args: call.arguments.clone(),
                sources,
            });
        } else if call.name == "bash"
            && !self
                .config
                .verification
                .commands
                .iter()
                .any(|c| call.arguments["command"] == c.command)
            && !approval::is_read_only(&self.ext, &call.name, &call.arguments)
        {
            // An arbitrary command can change unobserved state; do not turn it into executable memory.
            self.assistance.eligible = false;
        } else if matches!(call.name.as_str(), "solve" | "task")
            || (!matches!(
                call.name.as_str(),
                "todo" | "finish" | "experience" | "working_state" | "tool_load" | "bash"
            ) && !approval::is_read_only(&self.ext, &call.name, &call.arguments))
        {
            self.assistance.eligible = false;
        }
        if self.assistance.steps.len() > experience::MAX_STEPS {
            self.assistance.eligible = false;
            self.assistance.steps.truncate(experience::MAX_STEPS);
        }
        let ledger = serde_json::to_value(&self.assistance).unwrap_or(Value::Null);
        if experience::screen_record(&ledger).is_ok() {
            let mut control = self.session.control_state();
            control["investigation_ledger"] = ledger;
            let _ = self.session.set_control_state(control);
        }
    }
    pub(super) fn finish_experience(&mut self, ui: &mut dyn Ui) {
        let Some(store) = self.experience.clone() else {
            return;
        };
        let reused = self.assistance.reused.clone();
        let verified = !self.assistance.checks.is_empty();
        let credit_store = store.clone();
        thread::scope(|scope| {
            let (tx, rx) = mpsc::sync_channel(1);
            scope.spawn(move || {
                for id in reused {
                    let _ = credit_store.credit(&id, verified);
                }
                let _ = tx.send(());
            });
            let _ = receive_with_heartbeat(&rx, ui);
        });
        if !self.assistance.eligible
            || !self.assistance.mutated
            || self.assistance.steps.is_empty()
            || self.assistance.checks.is_empty()
        {
            return;
        }
        let mut recipe = Recipe {
            objective: self.assistance.objective.clone(),
            model: self.config.model.clone(),
            session: self.session.id().into(),
            steps: self.assistance.steps.clone(),
            checks: self.assistance.checks.clone(),
            lesson: None,
        };
        // A teacher only annotates observed success. It cannot supply checks, permissions or executable steps.
        if self.config.assistance.teacher_enabled
            && ui.maintenance_ready()
            && !self.interrupt.load(Ordering::SeqCst)
            && let Some(selection) = self.config.model_roles.get("coach")
        {
            let mut cfg = self.config.with_selection(selection);
            // Remote teacher calls never load a second local model or compete for its GPU.
            if !matches!(
                selection.provider.as_str(),
                "mlx" | "ollama" | "lm-studio" | "llama.cpp"
            ) {
                cfg.worker_timeout_secs = self.config.assistance.teacher_timeout_secs;
                let safe = json!({"objective":recipe.objective,"tools":recipe.steps.iter().map(|s|&s.tool).collect::<Vec<_>>(),"passing_configured_checks":recipe.checks});
                if crate::storage::memory::screen(&safe.to_string()).is_ok() {
                    let request = Chat {
                        messages: vec![
                            json!({"role":"system","content":"Return only JSON {\"lesson\":\"brief reusable guidance\"}. Describe limitations. Do not invent observed outcomes or executable instructions."}),
                            json!({"role":"user","content":safe.to_string()}),
                        ],
                        tools: vec![],
                        temperature: 0.0,
                        max_tokens: 256,
                        seed: None,
                    };
                    if let Ok(mut backend) = make_backend(&cfg) {
                        let cancel = Arc::new(AtomicBool::new(false));
                        let yield_cancel = cancel.clone();
                        let external = self.interrupt.clone();
                        ui.event(&crate::agent::ui_event::UiEvent::new(
                            crate::agent::ui_event::EventKind::Learning,
                            "Experience teacher: bounded optional review; coding has priority.",
                        ));
                        let result =
                            self.provider_request_with(&request, ui)
                                .and_then(|rewritten| {
                                    let request = rewritten.as_ref().unwrap_or(&request);
                                    thread::scope(|scope| {
                                        let (done, wait) = mpsc::channel::<()>();
                                        let watchdog = cancel.clone();
                                        let timeout = cfg.worker_timeout_secs;
                                        scope.spawn(move || {
                                            // 0 means no teacher deadline: wait only for completion or cancellation.
                                            let expired = if timeout == 0 {
                                                let _ = wait.recv();
                                                false
                                            } else {
                                                matches!(
                                                    wait.recv_timeout(Duration::from_secs(timeout)),
                                                    Err(RecvTimeoutError::Timeout)
                                                )
                                            };
                                            if expired {
                                                watchdog.store(true, Ordering::SeqCst);
                                            }
                                        });
                                        let started = std::time::Instant::now();
                                        let result = chat_with_ui(
                                            &mut backend,
                                            None,
                                            &request,
                                            ChatOptions {
                                                timeout,
                                                render_text: false,
                                                context_tokens: None,
                                                policy: None,
                                                recovery: Some(&cfg.request_recovery),
                                            },
                                            cancel,
                                            ui,
                                            &mut |ui| {
                                                if !ui.maintenance_ready()
                                                    || external.load(Ordering::SeqCst)
                                                {
                                                    yield_cancel.store(true, Ordering::SeqCst);
                                                }
                                            },
                                        );
                                        self.record_aux_request(&backend, &result, started);
                                        drop(done);
                                        result
                                    })
                                });
                        if let Ok(reply) = result
                            && let Ok(value) = serde_json::from_str::<Value>(&reply.text)
                            && let Some(lesson) = value["lesson"].as_str().filter(|s| {
                                s.len() <= 1024 && crate::storage::memory::screen(s).is_ok()
                            })
                        {
                            recipe.lesson = Some(lesson.into());
                        }
                        backend.release();
                    }
                }
            }
        }
        let saved = thread::scope(|scope| {
            let (tx, rx) = mpsc::sync_channel(1);
            scope.spawn(move || {
                let _ = tx.send(store.recipe(&recipe));
            });
            receive_with_heartbeat(&rx, ui).ok().and_then(Result::ok)
        });
        if saved.is_some() {
            ui.event(&crate::agent::ui_event::UiEvent::new(crate::agent::ui_event::EventKind::Learning, "Saved source-scoped recipe from observed edits and passing configured behavioral checks. Inspect /experience."));
        }
    }
    pub(super) fn experience_tool(
        &mut self,
        backend: &mut Backend,
        call: &ToolCall,
        ui: &mut dyn Ui,
    ) -> ToolOutput {
        let result = (|| -> Result<Value> {
            let store = self
                .experience
                .clone()
                .context("experience assistance is disabled")?;
            match call.arguments["action"].as_str().unwrap_or("inspect") {
                "inspect" => thread::scope(|scope| {
                    let (tx, rx) = mpsc::sync_channel(1);
                    scope.spawn(move || {
                        let _ = tx.send(store.inspect());
                    });
                    receive_with_heartbeat(&rx, ui)?
                }),
                "ledger" => Ok(serde_json::to_value(&self.assistance)?),
                "hypothesis" => {
                    let id = call.arguments["id"]
                        .as_str()
                        .context("an observed evidence ID is required")?;
                    let evidence = self
                        .assistance
                        .observations
                        .iter()
                        .find(|v| v["id"] == id)
                        .context("unknown observation")?;
                    let sources = serde_json::from_value(evidence["sources"].clone())?;
                    ensure!(
                        experience::applicable(&self.workspace, &sources),
                        "observation is stale; recheck source first"
                    );
                    let text = call.arguments["hypothesis"]
                        .as_str()
                        .context("hypothesis is required")?;
                    ensure!(text.len() <= 1024, "hypothesis exceeds budget");
                    crate::storage::memory::screen(text).map_err(anyhow::Error::msg)?;
                    let status = call.arguments["status"].as_str().unwrap_or("open");
                    ensure!(
                        ["open", "supported", "ruled_out"].contains(&status),
                        "invalid hypothesis status"
                    );
                    let value = json!({"evidence":id,"hypothesis":text,"status":status,"scope":"agent interpretation, not independently verified"});
                    self.assistance.hypotheses.push(value.clone());
                    if self.assistance.hypotheses.len() > 16 {
                        self.assistance.hypotheses.remove(0);
                    }
                    let mut state = self.session.control_state();
                    state["investigation_ledger"] = serde_json::to_value(&self.assistance)?;
                    self.session.set_control_state(state)?;
                    Ok(value)
                }
                "run" => {
                    ensure!(!self.plan_mode, "recipes cannot execute in plan mode");
                    let id = call.arguments["id"]
                        .as_str()
                        .context("recipe ID is required")?
                        .to_owned();
                    let recipe = store.get_recipe(&id)?;
                    for command in &recipe.checks {
                        ensure!(
                            self.config
                                .verification
                                .commands
                                .iter()
                                .any(|c| &c.command == command
                                    && self
                                        .config
                                        .verification
                                        .behavioral_checks
                                        .contains(&c.name)),
                            "recipe check is no longer configured as behavioral"
                        );
                    }
                    let catalog = self.catalog();
                    ensure!(
                        catalog.iter().any(|s| s == "bash"),
                        "recipe checks require allowed bash tool"
                    );
                    for step in &recipe.steps {
                        ensure!(
                            catalog.contains(&step.tool),
                            "recipe tool is not allowed by this model profile"
                        );
                    }
                    for (index, step) in recipe.steps.iter().enumerate() {
                        ensure!(!self.interrupt.load(Ordering::SeqCst), "recipe interrupted");
                        ensure!(
                            experience::applicable(&self.workspace, &step.sources),
                            "recipe preconditions changed at step {}; inspect and investigate before reuse",
                            index + 1
                        );
                        let nested = ToolCall {
                            id: format!("recipe:{id}:{index}"),
                            name: step.tool.clone(),
                            arguments: step.args.clone(),
                            malformed_arguments: false,
                        };
                        let sources = self.experience_sources(&nested, ui);
                        ui.tool_call(&nested.name, &summarize(&nested));
                        self.record(
                            self.cur_step,
                            "tool_call",
                            json!({"name":nested.name,"input":nested.arguments,"recipe":id}),
                        )?;
                        let out = self.dispatch(backend, &nested, ui);
                        self.record(self.cur_step, "tool_result", json!({"name":nested.name,"output":out.text,"is_error":out.is_error,"recipe":id}))?;
                        self.observe_experience(&nested, &out, sources, ui);
                        ui.tool_result(&nested.name, &out);
                        ensure!(
                            !out.is_error,
                            "recipe step {} failed: {}",
                            index + 1,
                            guard::clip_evidence(&out.text, 1024)
                        );
                        if step.tool == "write" {
                            self.step_checks.mutations += 1;
                        }
                    }
                    let mut passed = true;
                    for (index, command) in recipe.checks.iter().enumerate() {
                        ensure!(
                            self.config
                                .verification
                                .commands
                                .iter()
                                .any(|c| &c.command == command
                                    && self
                                        .config
                                        .verification
                                        .behavioral_checks
                                        .contains(&c.name)),
                            "recipe check is no longer configured as behavioral"
                        );
                        let nested = ToolCall {
                            id: format!("recipe-check:{id}:{index}"),
                            name: "bash".into(),
                            arguments: json!({"command":command}),
                            malformed_arguments: false,
                        };
                        ui.tool_call(&nested.name, &summarize(&nested));
                        self.record(
                            self.cur_step,
                            "tool_call",
                            json!({"name":nested.name,"input":nested.arguments,"recipe":id}),
                        )?;
                        let out = self.dispatch(backend, &nested, ui);
                        self.record(self.cur_step, "tool_result", json!({"name":nested.name,"output":out.text,"is_error":out.is_error,"recipe":id}))?;
                        ui.tool_result("bash", &out);
                        passed &= !out.is_error
                            && verification::exit_code(&out.text) == Some(0)
                            && !verification::zero_tests(&out.text);
                        if !passed {
                            break;
                        }
                    }
                    store.credit(&id, passed)?;
                    ensure!(
                        passed,
                        "recipe validation failed; edits retained for repair"
                    );
                    Ok(
                        json!({"recipe":id,"passed":true,"steps":recipe.steps.len(),"scope":"observed configured checks; not an external model evaluation"}),
                    )
                }
                other => bail!("unknown experience action {other}"),
            }
        })();
        match result {
            Ok(value) => ToolOutput {
                execution: Default::default(),
                data: None,
                text: value.to_string(),
                is_error: false,
            },
            Err(error) => ToolOutput {
                execution: Default::default(),
                data: None,
                text: format!("{error:#}"),
                is_error: true,
            },
        }
    }
    pub(super) fn preview_tool(&self, args: &Value, ui: &mut dyn Ui) -> ToolOutput {
        let ws = self.workspace.clone();
        let args = args.clone();
        let cancel = self.interrupt.clone();
        thread::scope(|scope| {
            let (tx, rx) = mpsc::sync_channel(1);
            scope.spawn(move || {
                let _ = tx.send(crate::tools::preview::run(&ws, &args, &cancel));
            });
            receive_with_heartbeat(&rx, ui).unwrap_or_else(|e| ToolOutput {
                execution: Default::default(),
                data: None,
                text: e.to_string(),
                is_error: true,
            })
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn candidates(entries: &[(&str, &str)]) -> Vec<(String, String)> {
        entries
            .iter()
            .map(|(name, description)| ((*name).into(), (*description).into()))
            .collect()
    }

    #[test]
    fn exact_name_is_authoritative_even_when_other_descriptions_match() {
        let matches = discover(
            "  LSP  ",
            candidates(&[
                ("lsp", "Inspect definitions and references"),
                ("other", "LSP compatible symbol search"),
            ]),
        );
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].0, "lsp");
        assert!(matches[0].2);
    }

    #[test]
    fn installed_mcp_inventory_is_discoverable_without_server_domain_words() {
        let config = Config::default();
        let loaded = crate::tools::mcp::McpLoad::default();
        let inventory = loaded.inventory_tool(&config);
        let description = inventory.schema()["function"]["description"]
            .as_str()
            .unwrap()
            .to_owned();
        let matches = discover(
            "list installed MCP servers",
            vec![
                (inventory.name().to_owned(), description),
                (
                    "mcp__memory__search_nodes".into(),
                    "memory: Search nodes".into(),
                ),
            ],
        );
        assert_eq!(matches[0].0, "mcp_servers");
        assert!(matches[0].2);
        assert!(
            !matches
                .iter()
                .any(|entry| entry.0 == "mcp__memory__search_nodes")
        );
    }

    #[test]
    fn natural_capability_queries_ignore_filler_not_domain_evidence() {
        let matches = discover(
            "I need a tool to inspect symbols and definitions please",
            candidates(&[
                (
                    "lsp",
                    "Inspect symbols, definitions and references in source",
                ),
                ("web_search", "Search public web sources"),
                ("read", "Read source files"),
            ]),
        );
        assert_eq!(
            matches
                .iter()
                .map(|entry| entry.0.as_str())
                .collect::<Vec<_>>(),
            ["lsp"]
        );
        assert!(matches[0].2);
    }

    #[test]
    fn strongest_name_evidence_wins_and_weaker_tier_is_not_activated() {
        let matches = discover(
            "web search",
            candidates(&[
                ("web_search", "Search public sources"),
                ("browser", "Browse and search web pages"),
                ("read", "Read files"),
            ]),
        );
        assert_eq!(matches[0].0, "web_search");
        assert!(matches[0].2);
        assert_eq!(matches[1].0, "browser");
        assert!(!matches[1].2);
        assert!(!matches.iter().any(|entry| entry.0 == "read"));
    }

    #[test]
    fn discovery_is_bounded_and_ties_are_independent_of_catalog_order() {
        let catalog: Vec<_> = (0..40)
            .map(|index| (format!("cap_{index:02}"), "Inspect widgets".into()))
            .collect();
        let first = discover("widgets", catalog.clone());
        let mut reversed = catalog;
        reversed.reverse();
        assert_eq!(first, discover("widgets", reversed));
        assert_eq!(first.len(), 8);
        assert_eq!(first.iter().filter(|entry| entry.2).count(), 3);
        assert_eq!(first[0].0, "cap_00");
    }

    #[test]
    fn empty_filler_and_unknown_queries_activate_nothing() {
        for query in ["", "  ", "I need a tool please", "quuxflorb"] {
            assert!(discover(query, candidates(&[("read", "Read files")])).is_empty());
        }
    }

    #[test]
    fn discovery_cannot_recover_profile_excluded_candidates() {
        let eligible = candidates(&[("read", "Read files")]);
        assert!(discover("web_search", eligible.clone()).is_empty());
        assert!(discover("search the public web", eligible).is_empty());
        assert!(discover("read", Vec::new()).is_empty());
    }

    #[test]
    fn task_and_skill_survive_compaction_without_new_enabled_state() {
        let loaded = HashSet::new();
        let mut names = vec![
            "find".into(),
            "skill".into(),
            "task".into(),
            "unrelated".into(),
        ];
        compact(&mut names, &loaded, 2, &BTreeMap::new());
        assert_eq!(names, ["find", "skill", "task"]);
        assert!(loaded.is_empty());
    }

    #[test]
    fn zero_budget_does_not_truncate_eligible_configured_tools() {
        let mut names = vec!["lsp".into(), "read".into(), "browser".into()];
        compact(&mut names, &HashSet::new(), 0, &BTreeMap::new());
        assert_eq!(names, ["browser", "lsp", "read"]);
    }

    #[test]
    fn explicit_tools_and_core_survive_budget() {
        let mut names = vec![
            "read".into(),
            "finish".into(),
            "tool_load".into(),
            "lsp".into(),
            "unused".into(),
        ];
        compact(
            &mut names,
            &HashSet::from(["lsp".into()]),
            2,
            &BTreeMap::new(),
        );
        assert!(names.contains(&"lsp".into()));
        assert!(names.contains(&"tool_load".into()));
        assert!(!names.contains(&"unused".into()));
    }
    #[test]
    fn check_failure_invalidates_capture() {
        let mut state = State::default();
        state.begin("repair");
        state.check("cargo test", true);
        assert_eq!(state.checks.len(), 1);
        state.check("cargo test", false);
        assert!(state.checks.is_empty());
    }
}
