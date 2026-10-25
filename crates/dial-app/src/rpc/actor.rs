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
    parked: &mut std::collections::HashMap<String, ParkedSession>,
    current: bool,
) {
    let previous_id = agent.session().id().to_owned();
    let previous = ParkedSession {
        plan: agent.plan_mode(),
        tools: agent.replace_session_tools(next.tools),
        session: agent.swap_session(next.session),
    };
    agent.set_plan_mode(next.plan);
    if current {
        parked.insert(previous_id, previous);
    }
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
    pub worker_questions: Arc<Mutex<Option<PendingQuestion>>>,
    pub worker_disconnected: Arc<AtomicBool>,
    pub worker_query_cache: Arc<Mutex<QueryProjection>>,
    pub worker_live: Arc<Mutex<LiveState>>,
    pub queue: Arc<Mutex<Vec<String>>>,
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
            worker_questions,
            worker_disconnected,
            worker_query_cache,
            worker_live,
            queue,
        } = self;
        let outcome = (|| -> Result<()> {
            let mut compute = None;
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
            agent.begin_session_lifecycle();
            if let Some(path) = std::env::var_os("DIAL_BENCHMARK_ADAPTER") {
                agent.set_benchmark_adapter(std::path::Path::new(&path))?;
            }
            query_snapshot(
                &agent,
                &mut worker_query_cache.lock().unwrap_or_else(|p| p.into_inner()),
            );
            let mut ui = RpcUi {
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
            let mut projected_messages = agent.session().message_count();
            let mut parked = std::collections::HashMap::<String, ParkedSession>::new();
            let mut current = !acp;
            loop {
                if worker_out.unavailable() {
                    break;
                }
                let request = match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(request) => request,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        agent.pump_subagents(&mut ui);
                        if current {
                            let queued = worker_prompt_queued.load(Ordering::SeqCst);
                            let foreground =
                                config.compute_locks && agent.foreground_idle_pending(queued)?;
                            let mut ready = !foreground || compute.is_some();
                            if foreground && compute.is_none() {
                                let lock = crate::learn::auto::lock_file(
                                    &layout.user_dir.join("compute.lock"),
                                )?;
                                match fs2::FileExt::try_lock_exclusive(&lock) {
                                    Ok(()) => {
                                        crate::learn::compute::register(
                                            &layout.user_dir,
                                            "session",
                                            &layout.project_root,
                                            &config.state_dir,
                                            &config.model,
                                        )?;
                                        compute = Some(lock);
                                        ready = true;
                                    }
                                    Err(error)
                                        if error.kind() == std::io::ErrorKind::WouldBlock => {}
                                    Err(error) => return Err(error.into()),
                                }
                            }
                            if ready && let Err(error) = agent.maintenance_tick(&mut ui, queued) {
                                ui.notice(&format!("Idle maintenance did not commit: {error:#}"));
                            }
                        }
                        if compute.is_some()
                            && (crate::learn::compute::background_pending(&layout.user_dir)?
                                || crate::learn::compute::foreground_pending(&layout.user_dir)?)
                            && agent.release_idle_models()
                            && let Some(lock) = compute.take()
                        {
                            crate::learn::compute::clear_owner(&layout.user_dir);
                            fs2::FileExt::unlock(&lock)?;
                        }
                        // Idle work (background workers, compaction) can change state without any actor
                        // command. Republish the projection only when runtime or history changed.
                        let messages = agent.session().message_count();
                        if std::mem::take(&mut ui.projection_dirty)
                            || messages != projected_messages
                        {
                            projected_messages = messages;
                            query_snapshot(
                                &agent,
                                &mut worker_query_cache.lock().unwrap_or_else(|p| p.into_inner()),
                            );
                        }
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                if worker_out.unavailable() {
                    break;
                }
                if matches!(request["method"].as_str(), Some("send" | "live_listen")) {
                    worker_prompt_queued.store(false, Ordering::SeqCst);
                    agent.note_compaction_input();
                }
                let id = request.get("id").cloned().unwrap_or(Value::Null);
                let params = &request["params"];
                let memory_compute = request["method"] == "memory"
                    && config.memory_uses_local_compute()
                    && !matches!(
                        params["command"]
                            .as_str()
                            .unwrap_or("status")
                            .split_whitespace()
                            .next(),
                        None | Some("status" | "queue")
                    );
                let result = (|| -> Result<Value> {
                    if compute.is_none()
                        && config.compute_locks
                        && (memory_compute
                            || (config.requires_compute_lock()
                                && matches!(
                                    request["method"].as_str(),
                                    Some("send" | "live_listen" | "compact" | "task_start")
                                )))
                    {
                        compute = crate::learn::auto::compute_lock_cancellable(&layout, || {
                            flag.load(Ordering::SeqCst) || worker_out.unavailable()
                        })?;
                        anyhow::ensure!(compute.is_some(), "compute wait cancelled");
                        crate::learn::compute::register(
                            &layout.user_dir,
                            "session",
                            &layout.project_root,
                            &config.state_dir,
                            &config.model,
                        )?;
                    }
                    let method = request["method"].as_str().unwrap_or("");
                    if acp
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
                            current || parked.contains_key(wanted),
                            "session is not open"
                        );
                        if !current || agent.session().id() != wanted {
                            stop_owned_tasks(&mut agent, &mut ui);
                            let session = parked.remove(wanted).context("session is not open")?;
                            activate_session(&mut agent, session, &mut parked, current);
                            current = true;
                        }
                        worker_out.emit(
                            json!({"event":"active_session","session_id":agent.session().id()}),
                        );
                    }
                    match request["method"].as_str().unwrap_or("") {
                        "send" | "live_listen" => {
                            let (spoken, transcript) = if request["method"] == "live_listen" {
                                let session = live_state(&worker_live)
                                    .session
                                    .clone()
                                    .context("no live voice session is active")?;
                                let heard = session.transcribe(&flag)?;
                                (heard.clone(), Some(heard))
                            } else {
                                let text = params["text"]
                                    .as_str()
                                    .context("send requires params.text")?;
                                (text.to_owned(), None)
                            };
                            let started = Instant::now();
                            let prompt = {
                                let mut state = live_state(&worker_live);
                                stop_speech(&mut state);
                                state
                                    .session
                                    .as_ref()
                                    .map(|session| session.prompt_for(&spoken))
                            };
                            let text = prompt.unwrap_or(spoken);
                            let turn = if acp {
                                agent.turn_prepared_preserving_interrupt(
                                    &text,
                                    params["_message"].clone(),
                                    &mut ui,
                                )
                            } else {
                                agent.turn_preserving_interrupt(&text, &mut ui)
                            };
                            let report = match turn {
                                Ok(report) => report,
                                Err(_) if acp && flag.load(Ordering::SeqCst) => {
                                    return Ok(
                                        json!({"interrupted":true,"plan_mode":agent.plan_mode()}),
                                    );
                                }
                                Err(error) => return Err(error),
                            };
                            if !report.interrupted {
                                speak_reply(&worker_live, &worker_out, &report.text);
                            }
                            // The turn spent its whole step budget; clients need that distinct from other incomplete turns.
                            let step_limit = report.outcome == crate::runtime::Phase::Incomplete
                                && config.harness.max_steps > 0
                                && report.steps >= config.harness.max_steps;
                            let mut result = json!({"text":report.text,"outcome":report.outcome,"steps":report.steps,"interrupted":report.interrupted,"duration_ms":started.elapsed().as_millis(),"session_id":agent.session().id(),"plan_mode":agent.plan_mode(),"metrics":agent.metrics(),"efficiency":agent.efficiency_report(),"speculation":agent.speculation_stats()});
                            result["step_limit"] = json!(step_limit);
                            if let Some(transcript) = transcript {
                                result["transcript"] = json!(transcript);
                            }
                            Ok(result)
                        }
                        "state" => Ok(state_projection(&agent)),
                        "plan" => {
                            agent.set_plan_mode(
                                params["enabled"]
                                    .as_bool()
                                    .context("plan requires params.enabled")?,
                            );
                            Ok(json!({"enabled":agent.plan_mode()}))
                        }
                        "new_session" => {
                            agent
                                .gate_session_change(
                                    "session_before_switch",
                                    json!({"reason":"new"}),
                                )
                                .map_err(anyhow::Error::msg)?;
                            let session = Session::create(
                                &layout,
                                SessionMeta {
                                    model: config.model.clone(),
                                },
                            )?;
                            if acp {
                                stop_owned_tasks(&mut agent, &mut ui);
                                let next = ParkedSession::fresh(&agent, session);
                                activate_session(&mut agent, next, &mut parked, current);
                                agent.set_plan_mode(false);
                                current = true;
                            } else {
                                agent.swap_session(session);
                                agent.set_plan_mode(false);
                            }
                            Ok(
                                json!({"session_id":agent.session().id(),"plan_mode":agent.plan_mode()}),
                            )
                        }
                        "compact" => {
                            let mut options: crate::config::compaction::CompactionOptions =
                                if params.is_null() {
                                    Default::default()
                                } else {
                                    serde_json::from_value(params.clone())
                                        .context("invalid compact options")?
                                };
                            anyhow::ensure!(
                                options.reason
                                    == crate::config::compaction::CompactionReason::Manual,
                                "RPC compact accepts only manual maintenance; automatic reasons are owned by the actor"
                            );
                            options.suppress_continuation = true;
                            let outcome = agent.compact_with_options(&mut ui, options)?;
                            Ok(json!({
                                "compacted":outcome.kind == crate::agent::compact::compaction::CompactionOutcomeKind::Compaction,
                                "context_reset":outcome.kind == crate::agent::compact::compaction::CompactionOutcomeKind::NewContext,
                                "outcome":outcome
                            }))
                        }
                        "memory" => {
                            let command = params["command"]
                                .as_str()
                                .context("memory requires params.command")?;
                            anyhow::ensure!(
                                command.len() <= MAX_MEMORY_COMMAND_BYTES,
                                "memory command exceeds {MAX_MEMORY_COMMAND_BYTES} bytes"
                            );
                            agent.selected_memory_command(command, &mut ui)
                        }
                        "seed_context" => {
                            let messages = params["messages"]
                                .as_array()
                                .context("seed_context requires params.messages")?;
                            anyhow::ensure!(
                                messages.len() <= MAX_SEED_MESSAGES,
                                "at most {MAX_SEED_MESSAGES} seed messages"
                            );
                            for text in messages {
                                let text =
                                    text.as_str().context("context entries must be strings")?;
                                anyhow::ensure!(
                                    text.len() <= MAX_SEED_ENTRY_BYTES,
                                    "context entry exceeds 1 MiB"
                                );
                                agent
                                    .session_mut()
                                    .append(json!({"role":"user","content":text}))?;
                            }
                            Ok(json!({"seeded":messages.len()}))
                        }
                        "resume" => {
                            let wanted = params["session_id"]
                                .as_str()
                                .context("resume requires params.session_id")?;
                            if !agent.session().id().starts_with(wanted) {
                                agent
                                    .gate_session_change(
                                        "session_before_switch",
                                        json!({"reason":"resume","target_session_id":wanted}),
                                    )
                                    .map_err(anyhow::Error::msg)?;
                            }
                            if acp {
                                if !current || agent.session().id() != wanted {
                                    let session = if let Some(session) = parked.remove(wanted) {
                                        session
                                    } else {
                                        let info = Session::find_prefix(&layout, wanted)?
                                            .into_iter()
                                            .find(|s| s.id == wanted)
                                            .context("session id not found in launched project")?;
                                        ParkedSession::fresh(&agent, Session::open(&info.path)?)
                                    };
                                    stop_owned_tasks(&mut agent, &mut ui);
                                    activate_session(&mut agent, session, &mut parked, current);
                                    current = true;
                                }
                                if params["_replay"] == true {
                                    worker_out.emit(json!({"event":"active_session","session_id":agent.session().id()}));
                                    for message in agent.session().messages() {
                                        if flag.load(Ordering::SeqCst)
                                            || worker_out.failed.load(Ordering::SeqCst)
                                        {
                                            break;
                                        }
                                        worker_out.emit(
                                            json!({"event":"replay_message","message":message}),
                                        );
                                    }
                                }
                                Ok(
                                    json!({"session_id":agent.session().id(),"plan_mode":agent.plan_mode()}),
                                )
                            } else {
                                let matching = Session::find_prefix(&layout, wanted)?;
                                anyhow::ensure!(
                                    matching.len() == 1,
                                    "session id missing or ambiguous"
                                );
                                if agent.session().id() != matching[0].id {
                                    agent.swap_session(Session::open(&matching[0].path)?);
                                    agent.set_plan_mode(false);
                                }
                                Ok(json!({"session_id":agent.session().id()}))
                            }
                        }
                        "fork_session" if acp => {
                            let wanted = params["session_id"]
                                .as_str()
                                .context("session_id required")?;
                            agent
                                .gate_session_change(
                                    "session_before_branch",
                                    json!({"reason":"fork","source_session_id":wanted}),
                                )
                                .map_err(anyhow::Error::msg)?;
                            let session = if current && agent.session().id() == wanted {
                                stop_owned_tasks(&mut agent, &mut ui);
                                agent.session().fork(&layout)?
                            } else if let Some(session) = parked.get(wanted) {
                                session.session.fork(&layout)?
                            } else {
                                let info = Session::find_prefix(&layout, wanted)?
                                    .into_iter()
                                    .find(|s| s.id == wanted)
                                    .context("session id not found in launched project")?;
                                Session::read(&info.path)?.fork(&layout)?
                            };
                            if !current || agent.session().id() != wanted {
                                stop_owned_tasks(&mut agent, &mut ui);
                            }
                            let next = ParkedSession::fresh(&agent, session);
                            activate_session(&mut agent, next, &mut parked, current);
                            agent.set_plan_mode(false);
                            current = true;
                            Ok(
                                json!({"session_id":agent.session().id(),"plan_mode":agent.plan_mode()}),
                            )
                        }
                        "close_session" if acp => {
                            let wanted = params["session_id"]
                                .as_str()
                                .context("session_id required")?;
                            if current && agent.session().id() == wanted {
                                stop_owned_tasks(&mut agent, &mut ui);
                                let tools = agent.fresh_session_tools();
                                agent.replace_session_tools(tools);
                                agent.swap_session(Session::ephemeral());
                                agent.set_plan_mode(false);
                                current = false;
                            } else {
                                parked.remove(wanted).context("session is not open")?;
                            }
                            Ok(json!({}))
                        }
                        "list_sessions" if acp => {
                            let offset = match params["cursor"].as_str() {
                                Some(cursor) => {
                                    cursor.parse::<usize>().context("invalid session cursor")?
                                }
                                None => 0,
                            };
                            // One lookahead row tells whether a next page exists without loading the whole history.
                            let sessions = Session::list(&layout, offset.saturating_add(101))?;
                            anyhow::ensure!(
                                offset <= sessions.len(),
                                "session cursor is past the end"
                            );
                            let page:Vec<_> = sessions.iter().skip(offset).take(100).map(|s|
                                json!({"sessionId":s.id,"cwd":layout.project_root,"title":s.title})).collect();
                            let next = offset + page.len();
                            let mut result = json!({"sessions":page});
                            if next < sessions.len() {
                                result["nextCursor"] = json!(next.to_string());
                            }
                            Ok(result)
                        }
                        "messages" => Ok(json!({"messages":agent.session().messages()})),
                        "rename" => {
                            agent.session_mut().rename(
                                params["title"]
                                    .as_str()
                                    .context("rename requires params.title")?,
                            )?;
                            Ok(json!({"title":agent.session().title()}))
                        }
                        "task_start" => agent.start_subagents(params, &mut ui),
                        "_cancel" if acp => Ok(json!({})),
                        "task_status" => agent.subagent_status(params["id"].as_str(), &mut ui),
                        _ => anyhow::bail!("unknown RPC method"),
                    }
                })();
                if acp && current && flag.load(Ordering::SeqCst) {
                    stop_owned_tasks(&mut agent, &mut ui);
                    let tools = agent.fresh_session_tools();
                    agent.replace_session_tools(tools);
                }
                if request["method"] == "close_session" {
                    worker_closing.store(false, Ordering::SeqCst);
                }
                query_snapshot(
                    &agent,
                    &mut worker_query_cache.lock().unwrap_or_else(|p| p.into_inner()),
                );
                if !worker_closing.load(Ordering::SeqCst) {
                    active.store(false, Ordering::SeqCst);
                }
                worker_out.result(&id, result);
                if worker_out.failed.load(Ordering::SeqCst) {
                    break;
                }
            }
            Ok(())
        })();
        if let Err(error) = &outcome {
            worker_out.emit(json!({"event":"fatal","message":format!("{error:#}")}));
        }
        outcome
    }
}
