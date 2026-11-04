//! Agent-owning side of the control protocol. Only this thread mutates sessions or runs
//! model/tool work; the controller remains responsive to queries, approvals and cancellation.
use super::*;

/// Keep each parked session's execution surface and mode together. Removing a session
/// cannot leave independently keyed tools or mode state behind.
struct ParkedSession {
    session: Session,
    tools: crate::tools::integrations::ext::Registry,
    plan: bool,
}
impl ParkedSession {
    fn fresh(agent: &Agent<'_>, session: Session) -> Self {
        Self {
            session,
            tools: agent.fresh_session_tools(),
            plan: false,
        }
    }
}

fn activate_session(
    agent: &mut Agent<'_>,
    next: ParkedSession,
    workspace: crate::agent::base::SessionWorkspace,
    parked: &mut std::collections::HashMap<String, ParkedSession>,
    current: bool,
) {
    let previous_id = agent.session().id().to_owned();
    let previous = ParkedSession {
        plan: agent.plan_mode(),
        tools: agent.replace_session_tools(next.tools),
        session: agent.swap_session_prepared(next.session, workspace),
    };
    agent.set_plan_mode(next.plan);
    if current {
        parked.insert(previous_id, previous);
    }
}

/// Host tools and schemes belong to the session they were declared in. Every session switch drops
/// them from the session being left (so no parked session carries them) and resets the host
/// registration, so the host must declare again after switching back.
fn drop_host_tools(
    agent: &mut Agent<'_>,
    bridge: &host_tools::HostBridge,
    uris: &host_uris::HostUris,
) {
    let names = bridge.reset();
    agent.unregister_host_tools(&names);
    uris.reset();
}

type Controls = (
    crate::agent::LiveTaskControl,
    crate::agent::compact::compaction_runtime::CompactionControl,
);

/// Owned endpoints transferred once from the controller to its actor thread.
/// The shared flags/projections are the existing control boundary, not a second agent owner.
pub(super) struct Actor {
    pub layout: Layout,
    pub config: Config,
    pub acp: bool,
    pub launch: crate::agent::modes::Launch,
    pub rx: mpsc::Receiver<Value>,
    pub approval_rx: mpsc::Receiver<(String, Decision)>,
    pub question_rx: mpsc::Receiver<(String, crate::agent::workflow::questions::QuestionOutcome)>,
    pub control_tx: mpsc::SyncSender<Controls>,
    pub worker_out: Sender,
    pub flag: Arc<AtomicBool>,
    pub active: Arc<AtomicBool>,
    pub worker_closing: Arc<AtomicBool>,
    pub worker_prompt_queued: Arc<AtomicBool>,
    pub worker_pending: Arc<Mutex<Option<String>>>,
    pub worker_host: Arc<host_tools::HostBridge>,
    pub worker_uris: Arc<host_uris::HostUris>,
    pub worker_questions: Arc<Mutex<Option<PendingQuestion>>>,
    pub worker_disconnected: Arc<AtomicBool>,
    pub worker_query_cache: Arc<Mutex<QueryProjection>>,
    pub worker_live: Arc<Mutex<LiveState>>,
    pub queue: Arc<Mutex<Vec<String>>>,
    pub worker_fs: Arc<client_fs::ClientFsCalls>,
}

/// A panic invalidates the actor's mutable state: reply once, emit fatal, then stop.
fn run_boundary(
    work: impl FnOnce(&mut Option<Value>) -> Result<()>,
    mut reply: impl FnMut(&Value, Result<Value>),
    mut fatal: impl FnMut(&str),
) -> Result<()> {
    let mut in_flight = None;
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&mut in_flight)))
        .unwrap_or_else(|_| {
            if let Some(id) = in_flight.take() {
                reply(
                    &id,
                    Err(anyhow::Error::new(RequestError {
                        code: "internal_error",
                        message: "RPC actor panicked; session terminated".into(),
                    })),
                );
            }
            Err(anyhow::anyhow!("RPC actor panicked; session terminated"))
        });
    if let Err(error) = &outcome {
        fatal(&format!("{error:#}"));
    }
    outcome
}

impl Actor {
    pub(super) fn run(self) -> Result<()> {
        let Self {
            layout,
            config,
            acp,
            launch,
            rx,
            approval_rx,
            question_rx,
            control_tx,
            worker_out,
            flag,
            active,
            worker_closing,
            worker_prompt_queued,
            worker_pending,
            worker_host,
            worker_uris,
            worker_questions,
            worker_disconnected,
            worker_query_cache,
            worker_live,
            queue,
            worker_fs,
        } = self;
        run_boundary(
            |in_flight| -> Result<()> {
                let backend = make_backend(&config)?;
                let image_input = backend.accepts_image_input();
                let session = if acp {
                    Session::ephemeral()
                } else {
                    Session::create(
                        &layout,
                        SessionMeta {
                            model: config.model.clone(),
                        },
                    )?
                };
                let mut agent = Agent::interactive(
                    &config,
                    backend,
                    layout.clone(),
                    session,
                    None,
                    None,
                    flag.clone(),
                )?;
                agent.set_launch(launch)?;
                if acp {
                    // Bridged only for what the client advertises at `initialize` (checked per call).
                    agent
                        .workspace_mut()
                        .set_client_files(Arc::new(client_fs::ClientFs::new(
                            worker_fs,
                            worker_out.clone(),
                            flag.clone(),
                        )));
                }
                agent.begin_session_lifecycle();
                if let Some(path) = std::env::var_os("DIAL_BENCHMARK_ADAPTER") {
                    agent.set_benchmark_adapter(std::path::Path::new(&path))?;
                }
                query_snapshot(
                    &agent,
                    &mut worker_query_cache.lock().unwrap_or_else(|p| p.into_inner()),
                );
                let ui = RpcUi {
                    out: worker_out.clone(),
                    approvals: approval_rx,
                    pending_approval: worker_pending,
                    question_answers: question_rx,
                    pending_question: worker_questions,
                    interrupt: flag.clone(),
                    input_disconnected: worker_disconnected,
                    steering: queue,
                    prompt_queued: worker_prompt_queued.clone(),
                    next: 0,
                    node_cursors: Default::default(),
                    node_revisions: Default::default(),
                    node_meta: Default::default(),
                    projection_dirty: false,
                    tree: None,
                };
                worker_out.emit(ready_frame(
                    agent.session().id(),
                    &config.model,
                    json!(agent.tool_names()),
                    image_input,
                    acp,
                ));
                let _ = control_tx.send((
                    agent.live_task_control(),
                    agent.background_compaction_control(),
                ));
                // Tracks the message count the published projection was built from.
                let projected_messages = agent.session().message_count();
                let parked = std::collections::HashMap::<String, ParkedSession>::new();
                let current = !acp;
                let mut runtime = ActorRuntime {
                    layout: &layout,
                    config: &config,
                    acp,
                    rx,
                    worker_out: &worker_out,
                    flag,
                    active,
                    worker_closing,
                    worker_prompt_queued,
                    worker_host,
                    worker_uris,
                    worker_query_cache,
                    worker_live,
                    agent,
                    ui,
                    compute: None,
                    projected_messages,
                    projected_at: std::time::Instant::now(),
                    parked,
                    current,
                };
                runtime.run(in_flight)
            },
            |id, result| worker_out.result(id, result),
            |message| {
                worker_out.emit(json!({"event":"fatal","message":message}));
            },
        )
    }
}

/// Session and execution state owned by the actor thread. The controller still communicates only
/// through the existing channels, flags and query projection.
/// Shortest gap between projection rebuilds driven only by runtime (worker) changes.
const PROJECTION_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

struct ActorRuntime<'a> {
    // Fields drop in declaration order: parked sessions and UI before the agent, then its
    // compute lease, matching the former run-local cleanup order on errors and panics.
    parked: std::collections::HashMap<String, ParkedSession>,
    ui: RpcUi,
    agent: Agent<'a>,
    compute: Option<std::fs::File>,
    layout: &'a Layout,
    config: &'a Config,
    acp: bool,
    rx: mpsc::Receiver<Value>,
    worker_out: &'a Sender,
    flag: Arc<AtomicBool>,
    active: Arc<AtomicBool>,
    worker_closing: Arc<AtomicBool>,
    worker_prompt_queued: Arc<AtomicBool>,
    worker_host: Arc<host_tools::HostBridge>,
    worker_uris: Arc<host_uris::HostUris>,
    worker_query_cache: Arc<Mutex<QueryProjection>>,
    worker_live: Arc<Mutex<LiveState>>,
    projected_messages: usize,
    /// When the query projection was last rebuilt.
    projected_at: std::time::Instant,
    current: bool,
}

impl ActorRuntime<'_> {
    fn run(&mut self, in_flight: &mut Option<Value>) -> Result<()> {
        loop {
            if self.worker_out.unavailable() {
                break;
            }
            let request = match self.rx.recv_timeout(Duration::from_millis(100)) {
                Ok(request) => request,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.idle_tick()?;
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            if self.worker_out.unavailable() {
                break;
            }
            if matches!(request["method"].as_str(), Some("send" | "live_listen")) {
                self.worker_prompt_queued.store(false, Ordering::SeqCst);
                self.agent.note_compaction_input();
            }
            let id = request.get("id").cloned().unwrap_or(Value::Null);
            *in_flight = Some(id.clone());
            let result = self.handle_request(&request);
            self.settle_request(&request, &id, result);
            *in_flight = None;
            if self.worker_out.failed.load(Ordering::SeqCst) {
                break;
            }
        }
        Ok(())
    }

    /// Publish the post-request state and reply only after cancellation and close cleanup.
    fn settle_request(&mut self, request: &Value, id: &Value, result: Result<Value>) {
        if self.acp && self.current && self.flag.load(Ordering::SeqCst) {
            stop_owned_tasks(&mut self.agent, &mut self.ui);
            let tools = self.agent.fresh_session_tools();
            self.agent.replace_session_tools(tools);
        }
        if request["method"] == "close_session" {
            self.worker_closing.store(false, Ordering::SeqCst);
        }
        query_snapshot(
            &self.agent,
            &mut self
                .worker_query_cache
                .lock()
                .unwrap_or_else(|p| p.into_inner()),
        );
        if !self.worker_closing.load(Ordering::SeqCst) {
            self.active.store(false, Ordering::SeqCst);
        }
        self.worker_out.result(id, result);
    }

    fn idle_tick(&mut self) -> Result<()> {
        self.agent.pump_subagents(&mut self.ui);
        if self.current {
            let queued = self.worker_prompt_queued.load(Ordering::SeqCst);
            let foreground =
                self.config.compute_locks && self.agent.foreground_idle_pending(queued)?;
            let mut ready = !foreground || self.compute.is_some();
            if foreground && self.compute.is_none() {
                let lock = crate::learn::auto::lock_file(
                    &crate::config::paths::compute_lock_file(&self.layout.user_dir),
                )?;
                match std::fs::File::try_lock(&lock).map_err(std::io::Error::from) {
                    Ok(()) => {
                        crate::learn::compute::register(
                            &self.layout.user_dir,
                            "session",
                            &self.layout.project_root,
                            &self.config.state_dir,
                            &self.config.model,
                        )?;
                        self.compute = Some(lock);
                        ready = true;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) => return Err(error.into()),
                }
            }
            if ready && let Err(error) = self.agent.maintenance_tick(&mut self.ui, queued) {
                self.ui
                    .notice(&format!("Idle maintenance did not commit: {error:#}"));
            }
        }
        if self.compute.is_some()
            && (crate::learn::compute::background_pending(&self.layout.user_dir)?
                || crate::learn::compute::foreground_pending(&self.layout.user_dir)?)
            && self.agent.release_idle_models()
            && let Some(lock) = self.compute.take()
        {
            crate::learn::compute::clear_owner(&self.layout.user_dir);
            std::fs::File::unlock(&lock)?;
        }
        // Idle work (background workers, compaction) can change state without any actor
        // command. Republish the projection only when runtime or history changed.
        let messages = self.agent.session().message_count();
        // Streaming workers mark the projection dirty on almost every idle tick, and a rebuild
        // copies the whole execution tree under the cache lock. Runtime-only changes republish at
        // most every PROJECTION_INTERVAL; a history change still republishes at once.
        let runtime_due =
            self.ui.projection_dirty && self.projected_at.elapsed() >= PROJECTION_INTERVAL;
        if runtime_due || messages != self.projected_messages {
            self.ui.projection_dirty = false;
            self.projected_at = std::time::Instant::now();
            self.projected_messages = messages;
            query_snapshot(
                &self.agent,
                &mut self
                    .worker_query_cache
                    .lock()
                    .unwrap_or_else(|p| p.into_inner()),
            );
        }
        Ok(())
    }

    /// Implicit ACP selection is a real session switch, including for query requests.
    fn activate_parked(&mut self, wanted: &str) -> Result<()> {
        let next = self.parked.get(wanted).context("session is not open")?;
        self.agent
            .gate_session_change(
                "session_before_switch",
                json!({"reason":"activate","target_session_id":wanted}),
            )
            .map_err(anyhow::Error::msg)?;
        let workspace = self.agent.prepare_session_workspace(&next.session)?;
        stop_owned_tasks(&mut self.agent, &mut self.ui);
        drop_host_tools(&mut self.agent, &self.worker_host, &self.worker_uris);
        let next = self.parked.remove(wanted).context("session is not open")?;
        activate_session(
            &mut self.agent,
            next,
            workspace,
            &mut self.parked,
            self.current,
        );
        self.current = true;
        Ok(())
    }

    fn handle_request(&mut self, request: &Value) -> Result<Value> {
        let params = &request["params"];
        let method = request["method"].as_str().unwrap_or("");
        if self.acp
            && !matches!(
                method,
                "new_session"
                    | "resume"
                    | "fork_session"
                    | "list_sessions"
                    | "close_session"
                    | "_cancel"
            )
        {
            let wanted = params["session_id"]
                .as_str()
                .context("session_id required")?;
            anyhow::ensure!(
                self.current || self.parked.contains_key(wanted),
                "session is not open"
            );
            if !self.current || self.agent.session().id() != wanted {
                self.activate_parked(wanted)?;
            }
            self.worker_out
                .emit(json!({"event":"active_session","session_id":self.agent.session().id()}));
        }
        let memory_compute = request["method"] == "memory"
            && self.config.memory_uses_local_compute()
            && !matches!(
                params["command"]
                    .as_str()
                    .unwrap_or("status")
                    .split_whitespace()
                    .next(),
                None | Some("status" | "queue")
            );
        if self.compute.is_none()
            && self.config.compute_locks
            && (memory_compute
                || (self.config.requires_compute_lock()
                    && matches!(
                        request["method"].as_str(),
                        Some("send" | "live_listen" | "compact" | "task_start")
                    )))
        {
            self.compute = crate::learn::auto::compute_lock_cancellable(self.layout, || {
                self.flag.load(Ordering::SeqCst) || self.worker_out.unavailable()
            })?;
            anyhow::ensure!(self.compute.is_some(), "compute wait cancelled");
            crate::learn::compute::register(
                &self.layout.user_dir,
                "session",
                &self.layout.project_root,
                &self.config.state_dir,
                &self.config.model,
            )?;
        }
        match request["method"].as_str().unwrap_or("") {
            "send" | "live_listen" => self.send(request, params),
            "state" => Ok(state_projection(&self.agent)),
            "plan" => {
                self.agent.set_plan_mode(
                    params["enabled"]
                        .as_bool()
                        .context("plan requires params.enabled")?,
                );
                Ok(json!({"enabled":self.agent.plan_mode()}))
            }
            "set_model" => {
                let provider = params["provider"]
                    .as_str()
                    .context("set_model requires params.provider")?;
                let model_id = params["model_id"]
                    .as_str()
                    .context("set_model requires params.model_id")?;
                let selection = crate::agent::modes::switch::resolve(
                    self.agent.effective_config(),
                    &format!("{provider}/{model_id}"),
                )?;
                anyhow::ensure!(
                    crate::agent::modes::switch::is_known_model(
                        self.agent.effective_config(),
                        &self.layout.user_dir,
                        &selection,
                    ),
                    "unknown model {provider}/{model_id}: not in the catalog, configured models or downloaded local models"
                );
                self.agent.switch_model(&selection, None)?;
                Ok(json!({"model":self.agent.effective_config().model}))
            }
            "set_thinking" => {
                let level = params["level"]
                    .as_str()
                    .context("set_thinking requires params.level")?;
                let effort = crate::agent::modes::switch::parse_effort(level)
                    .with_context(|| format!("unknown reasoning level: {level}"))?;
                let selection = self.agent.effective_config().selection();
                self.agent.switch_model(&selection, Some(effort))?;
                Ok(json!({"level":effort.as_str()}))
            }
            "get_session_stats" => {
                let state = state_projection(&self.agent);
                Ok(json!({
                    "session_id":state["session_id"],
                    "messages":self.agent.session().messages().len(),
                    "cost":state["cost"],
                    "observed_cost":state["observed_cost"],
                    "usage":state["usage"],
                    "metrics":state["metrics"]
                }))
            }
            "get_available_commands" => {
                let commands: Vec<Value> = crate::app::builtin_commands()
                .iter()
                .map(|(name, usage, description)| {
                    json!({"name":name,"usage":usage,"description":description})
                })
                .collect();
                Ok(json!({"commands":commands}))
            }
            "export" => self.export(params),
            "set_host_tools" => self.set_host_tools(params),
            "set_host_uri_schemes" => self.set_host_uri_schemes(params),
            "new_session" => self.new_session(),
            "compact" => self.compact(params),
            "memory" => {
                let command = params["command"]
                    .as_str()
                    .context("memory requires params.command")?;
                anyhow::ensure!(
                    command.len() <= MAX_MEMORY_COMMAND_BYTES,
                    "memory command exceeds {MAX_MEMORY_COMMAND_BYTES} bytes"
                );
                self.agent.selected_memory_command(command, &mut self.ui)
            }
            "seed_context" => self.seed_context(params),
            "resume" => self.resume(params),
            "fork_session" if self.acp => self.fork_session(params),
            "close_session" if self.acp => self.close_session(params),
            "list_sessions" if self.acp => self.list_sessions(params),
            "messages" => Ok(json!({"messages":self.agent.session().messages()})),
            "rename" => {
                self.agent.session_mut().rename(
                    params["title"]
                        .as_str()
                        .context("rename requires params.title")?,
                )?;
                Ok(json!({"title":self.agent.session().title()}))
            }
            "task_start" => self.agent.start_subagents(params, &mut self.ui),
            "_cancel" if self.acp => Ok(json!({})),
            "task_status" => self
                .agent
                .subagent_status(params["id"].as_str(), &mut self.ui),
            _ => Err(anyhow::Error::new(RequestError {
                code: "unknown_method",
                message: "unknown RPC method".to_owned(),
            })),
        }
    }

    fn send(&mut self, request: &Value, params: &Value) -> Result<Value> {
        let (spoken, transcript) = if request["method"] == "live_listen" {
            let session = live_state(&self.worker_live)
                .session
                .clone()
                .context("no live voice session is active")?;
            let heard = session.transcribe(&self.flag)?;
            (heard.clone(), Some(heard))
        } else {
            let text = params["text"]
                .as_str()
                .context("send requires params.text")?;
            (text.to_owned(), None)
        };
        let started = Instant::now();
        let prompt = {
            let mut state = live_state(&self.worker_live);
            stop_speech(&mut state);
            state
                .session
                .as_ref()
                .map(|session| session.prompt_for(&spoken))
        };
        let text = prompt.unwrap_or(spoken);
        let turn = if self.acp {
            self.agent.turn_prepared_preserving_interrupt(
                &text,
                params["_message"].clone(),
                &mut self.ui,
            )
        } else {
            self.agent.turn_preserving_interrupt(&text, &mut self.ui)
        };
        let report = match turn {
            Ok(report) => report,
            Err(_) if self.acp && self.flag.load(Ordering::SeqCst) => {
                return Ok(json!({"interrupted":true,"plan_mode":self.agent.plan_mode()}));
            }
            Err(error) => return Err(error),
        };
        if !report.interrupted {
            speak_reply(&self.worker_live, self.worker_out, &report.text);
        }
        // The turn spent its whole step budget; clients need that distinct from other incomplete turns.
        let step_limit = report.outcome == crate::runtime::Phase::Incomplete
            && self.config.harness.max_steps > 0
            && report.steps >= self.config.harness.max_steps;
        let mut result = json!({"text":report.text,"outcome":report.outcome,"steps":report.steps,"interrupted":report.interrupted,"duration_ms":started.elapsed().as_millis(),"session_id":self.agent.session().id(),"plan_mode":self.agent.plan_mode(),"metrics":self.agent.metrics(),"efficiency":self.agent.efficiency_report(),"speculation":self.agent.speculation_stats()});
        result["step_limit"] = json!(step_limit);
        if let Some(transcript) = transcript {
            result["transcript"] = json!(transcript);
        }
        Ok(result)
    }

    fn export(&self, params: &Value) -> Result<Value> {
        let format = params["format"].as_str().unwrap_or("html");
        let html = if format == "html" {
            true
        } else if format == "markdown" {
            false
        } else {
            anyhow::bail!("unsupported export format: {format}");
        };
        let name = match params["path"].as_str() {
            Some(path) => path.to_owned(),
            None => format!(
                "dial-session-{}.{}",
                self.agent.session().id(),
                if html { "html" } else { "md" }
            ),
        };
        let relative = std::path::Path::new(&name);
        anyhow::ensure!(
            relative
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_))),
            "export path must be relative to the project root without '..'"
        );
        self.agent
            .session()
            .export(self.layout, &self.layout.project_root.join(relative), html)?;
        Ok(json!({"path":name,"format":format}))
    }

    fn set_host_tools(&mut self, params: &Value) -> Result<Value> {
        let specs = host_tools::parse_specs(&params["tools"])?;
        self.worker_host.ensure_unregistered()?;
        let names: Vec<String> = specs.iter().map(|spec| spec.name.clone()).collect();
        let tools: Vec<Arc<dyn crate::tools::integrations::ext::Tool>> = specs
            .into_iter()
            .map(|spec| {
                Arc::new(host_tools::HostTool::new(
                    spec,
                    self.worker_host.clone(),
                    self.worker_out.clone(),
                    self.flag.clone(),
                )) as Arc<dyn crate::tools::integrations::ext::Tool>
            })
            .collect();
        self.agent.register_host_tools(tools)?;
        self.worker_host.mark_registered(names.clone());
        Ok(json!({"tools": names}))
    }

    fn set_host_uri_schemes(&mut self, params: &Value) -> Result<Value> {
        let schemes = host_uris::parse_schemes(&params["schemes"])?;
        self.worker_uris.declare(schemes.clone());
        self.agent
            .workspace_mut()
            .set_host_uris(Some(Arc::new(host_uris::Resolver::new(
                self.worker_uris.clone(),
                self.worker_out.clone(),
            ))));
        Ok(json!({"schemes": schemes}))
    }

    fn new_session(&mut self) -> Result<Value> {
        self.agent
            .gate_session_change("session_before_switch", json!({"reason":"new"}))
            .map_err(anyhow::Error::msg)?;
        let session = Session::create(
            self.layout,
            SessionMeta {
                model: self.config.model.clone(),
            },
        )?;
        let workspace = self.agent.prepare_session_workspace(&session)?;
        drop_host_tools(&mut self.agent, &self.worker_host, &self.worker_uris);
        if self.acp {
            stop_owned_tasks(&mut self.agent, &mut self.ui);
            let next = ParkedSession::fresh(&self.agent, session);
            activate_session(
                &mut self.agent,
                next,
                workspace,
                &mut self.parked,
                self.current,
            );
            self.agent.set_plan_mode(false);
            self.current = true;
        } else {
            self.agent.swap_session_prepared(session, workspace);
            self.agent.set_plan_mode(false);
        }
        Ok(json!({"session_id":self.agent.session().id(),"plan_mode":self.agent.plan_mode()}))
    }

    fn compact(&mut self, params: &Value) -> Result<Value> {
        let mut options: crate::config::compaction::CompactionOptions = if params.is_null() {
            Default::default()
        } else {
            serde_json::from_value(params.clone()).context("invalid compact options")?
        };
        anyhow::ensure!(
            options.reason == crate::config::compaction::CompactionReason::Manual,
            "RPC compact accepts only manual maintenance; automatic reasons are owned by the actor"
        );
        options.suppress_continuation = true;
        let outcome = self.agent.compact_with_options(&mut self.ui, options)?;
        Ok(json!({
            "compacted":outcome.kind == crate::agent::compact::compaction::CompactionOutcomeKind::Compaction,
            "context_reset":outcome.kind == crate::agent::compact::compaction::CompactionOutcomeKind::NewContext,
            "outcome":outcome
        }))
    }

    fn seed_context(&mut self, params: &Value) -> Result<Value> {
        let messages = params["messages"]
            .as_array()
            .context("seed_context requires params.messages")?;
        anyhow::ensure!(
            messages.len() <= MAX_SEED_MESSAGES,
            "at most {MAX_SEED_MESSAGES} seed messages"
        );
        for text in messages {
            let text = text.as_str().context("context entries must be strings")?;
            anyhow::ensure!(
                text.len() <= MAX_SEED_ENTRY_BYTES,
                "context entry exceeds 1 MiB"
            );
            self.agent
                .session_mut()
                .append(json!({"role":"user","content":text}))?;
        }
        Ok(json!({"seeded":messages.len()}))
    }

    fn resume(&mut self, params: &Value) -> Result<Value> {
        let wanted = params["session_id"]
            .as_str()
            .context("resume requires params.session_id")?;
        if !self.current
            || (if self.acp {
                self.agent.session().id() != wanted
            } else {
                !self.agent.session().id().starts_with(wanted)
            })
        {
            self.agent
                .gate_session_change(
                    "session_before_switch",
                    json!({"reason":"resume","target_session_id":wanted}),
                )
                .map_err(anyhow::Error::msg)?;
        }
        let mut forked_from: Option<String> = None;
        if self.acp {
            if !self.current || self.agent.session().id() != wanted {
                let workspace = self
                    .parked
                    .get(wanted)
                    .map(|next| self.agent.prepare_session_workspace(&next.session))
                    .transpose()?;
                let (session, workspace) = if let Some(workspace) = workspace {
                    (
                        self.parked.remove(wanted).context("session is not open")?,
                        workspace,
                    )
                } else {
                    let info = Session::find_prefix(self.layout, wanted)?
                        .into_iter()
                        .find(|s| s.id == wanted)
                        .context("session id not found in launched project")?;
                    let (session, source) = match Session::open_or_fork(self.layout, &info.path)? {
                        crate::storage::session::Resumed::Owned(session) => (session, None),
                        crate::storage::session::Resumed::Forked { source_id, session } => {
                            (session, Some(source_id))
                        }
                    };
                    forked_from = source;
                    let workspace = self.agent.prepare_session_workspace(&session)?;
                    (ParkedSession::fresh(&self.agent, session), workspace)
                };
                stop_owned_tasks(&mut self.agent, &mut self.ui);
                drop_host_tools(&mut self.agent, &self.worker_host, &self.worker_uris);
                activate_session(
                    &mut self.agent,
                    session,
                    workspace,
                    &mut self.parked,
                    self.current,
                );
                self.current = true;
            }
            if params["_replay"] == true {
                self.worker_out
                    .emit(json!({"event":"active_session","session_id":self.agent.session().id()}));
                for message in self.agent.session().messages() {
                    if self.flag.load(Ordering::SeqCst)
                        || self.worker_out.failed.load(Ordering::SeqCst)
                    {
                        break;
                    }
                    self.worker_out
                        .emit(json!({"event":"replay_message","message":message}));
                }
            }
            Ok(
                json!({"session_id":self.agent.session().id(),"plan_mode":self.agent.plan_mode(),"forked_from":forked_from}),
            )
        } else {
            let matching = Session::find_prefix(self.layout, wanted)?;
            anyhow::ensure!(matching.len() == 1, "session id missing or ambiguous");
            if self.agent.session().id() != matching[0].id {
                let (session, source) = match Session::open_or_fork(self.layout, &matching[0].path)?
                {
                    crate::storage::session::Resumed::Owned(session) => (session, None),
                    crate::storage::session::Resumed::Forked { source_id, session } => {
                        (session, Some(source_id))
                    }
                };
                forked_from = source;
                let workspace = self.agent.prepare_session_workspace(&session)?;
                stop_owned_tasks(&mut self.agent, &mut self.ui);
                drop_host_tools(&mut self.agent, &self.worker_host, &self.worker_uris);
                self.agent.swap_session_prepared(session, workspace);
                self.agent.set_plan_mode(false);
            }
            Ok(json!({"session_id":self.agent.session().id(),"forked_from":forked_from}))
        }
    }

    fn fork_session(&mut self, params: &Value) -> Result<Value> {
        let wanted = params["session_id"]
            .as_str()
            .context("session_id required")?;
        self.agent
            .gate_session_change(
                "session_before_branch",
                json!({"reason":"fork","source_session_id":wanted}),
            )
            .map_err(anyhow::Error::msg)?;
        self.agent
            .gate_session_change(
                "session_before_switch",
                json!({"reason":"fork","source_session_id":wanted}),
            )
            .map_err(anyhow::Error::msg)?;
        let session = if self.current && self.agent.session().id() == wanted {
            self.agent.session().fork(self.layout)?
        } else if let Some(session) = self.parked.get(wanted) {
            session.session.fork(self.layout)?
        } else {
            let info = Session::find_prefix(self.layout, wanted)?
                .into_iter()
                .find(|s| s.id == wanted)
                .context("session id not found in launched project")?;
            Session::read(&info.path)?.fork(self.layout)?
        };
        let workspace = self.agent.prepare_session_workspace(&session)?;
        stop_owned_tasks(&mut self.agent, &mut self.ui);
        drop_host_tools(&mut self.agent, &self.worker_host, &self.worker_uris);
        let next = ParkedSession::fresh(&self.agent, session);
        activate_session(
            &mut self.agent,
            next,
            workspace,
            &mut self.parked,
            self.current,
        );
        self.agent.set_plan_mode(false);
        self.current = true;
        Ok(json!({"session_id":self.agent.session().id(),"plan_mode":self.agent.plan_mode()}))
    }

    fn close_session(&mut self, params: &Value) -> Result<Value> {
        let wanted = params["session_id"]
            .as_str()
            .context("session_id required")?;
        if self.current && self.agent.session().id() == wanted {
            self.agent
                .gate_session_change("session_before_switch", json!({"reason":"close"}))
                .map_err(anyhow::Error::msg)?;
            let session = Session::ephemeral();
            let workspace = self.agent.prepare_session_workspace(&session)?;
            stop_owned_tasks(&mut self.agent, &mut self.ui);
            drop_host_tools(&mut self.agent, &self.worker_host, &self.worker_uris);
            let tools = self.agent.fresh_session_tools();
            self.agent.replace_session_tools(tools);
            self.agent.swap_session_prepared(session, workspace);
            self.agent.set_plan_mode(false);
            self.current = false;
        } else {
            self.parked.remove(wanted).context("session is not open")?;
        }
        Ok(json!({}))
    }

    fn list_sessions(&self, params: &Value) -> Result<Value> {
        let offset = match params["cursor"].as_str() {
            Some(cursor) => cursor.parse::<usize>().context("invalid session cursor")?,
            None => 0,
        };
        // One lookahead row tells whether a next page exists without loading the whole history.
        let sessions = Session::list(self.layout, offset.saturating_add(101))?;
        anyhow::ensure!(offset <= sessions.len(), "session cursor is past the end");
        let page: Vec<_> = sessions
            .iter()
            .skip(offset)
            .take(100)
            .map(|s| json!({"sessionId":s.id,"cwd":self.layout.project_root,"title":s.title}))
            .collect();
        let next = offset + page.len();
        let mut result = json!({"sessions":page});
        if next < sessions.len() {
            result["nextCursor"] = json!(next.to_string());
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests;
