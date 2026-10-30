//! Laya advice, codebase context recall and architecture synthesis.

use crate::agent::*;

pub(in crate::agent) fn defer_architecture_attempt_on_failure<T>(
    cache: &Codebase,
    synthesis: &crate::tools::knowledge::codebase::architecture::Synthesis,
    result: Result<T>,
) -> Result<T> {
    if result.is_err() {
        cache.defer_architecture_attempt(synthesis)?;
    }
    result
}

impl<'a> Agent<'a> {
    /// Pre-turn retrieval-scope advice. Source hashing and indexing never run on the interactive
    /// input/render thread.
    pub(in crate::agent) fn laya_advice(
        &mut self,
        evidence: &str,
        ui: &mut dyn Ui,
    ) -> Option<String> {
        const PHASE: &str = "investigation";
        if self.hermetic || !self.config.laya.enabled || self.interrupt.load(Ordering::SeqCst) {
            return None;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if self.session.control_state()["laya"]["retry_after"]
            .as_u64()
            .is_some_and(|retry| now < retry)
        {
            return None;
        }
        ui.laya_activity("checking", "Choosing a retrieval scope · advisory");
        let config = self.config.laya_service();
        let cancel = self.interrupt.clone();
        let evidence = evidence.to_owned();
        let phase_owned = PHASE.to_owned();
        let result = thread::scope(|scope| -> Result<_> {
            let (tx, rx) = mpsc::sync_channel(1);
            scope.spawn(move || {
                let _ = tx.send(crate::agent::model::laya::predict(
                    &config,
                    &phase_owned,
                    &evidence,
                    cancel,
                ));
            });
            receive_with_heartbeat(&rx, ui)?
        });
        let mut state = self.session.control_state();
        match result {
            Ok(decision) => {
                let summary = crate::agent::model::laya::summary(PHASE, &decision.answers);
                let record = json!({"status":"ready","mode":"advisory","phase":PHASE,"checkpoint":decision.health.checkpoint,"device":decision.health.device,"answers":decision.answers,"latency_ms":decision.elapsed_ms,"calibration":"raw; not calibrated in Dial","summary":summary});
                if let Err(error) = self.record(self.cur_step, "laya_decision", record.clone()) {
                    ui.notice(&format!("Laya decision was not recorded: {error:#}"));
                }
                state["laya"] = record;
                if let Err(error) = self.session.set_control_state(state) {
                    ui.notice(&format!("Laya state was not saved: {error:#}"));
                }
                ui.laya_activity(
                    "advice",
                    &format!(
                        "{summary}\nCheckpoint: {} · {} ms",
                        decision.health.checkpoint, decision.elapsed_ms
                    ),
                );
                Some(summary)
            }
            Err(error) => {
                let reason = if self.interrupt.load(Ordering::SeqCst) {
                    "Cancelled".to_owned()
                } else {
                    format!("{error:#}")
                };
                let retry_after =
                    if reason.contains("exceeded its") || reason.contains("Laya is busy") {
                        Some(now.saturating_add(60))
                    } else {
                        None
                    };
                let status = if reason.contains("exceeded its") {
                    "delayed"
                } else if reason.contains("Laya is busy") {
                    "busy"
                } else if reason.contains("loading its checkpoint") {
                    "loading"
                } else {
                    "unavailable"
                };
                state["laya"] = json!({"status":status,"phase":PHASE,"reason":reason,"retry_after":retry_after});
                if let Err(error) = self.session.set_control_state(state) {
                    ui.notice(&format!("Laya state was not saved: {error:#}"));
                }
                if let Err(error) = self.record(
                    self.cur_step,
                    "laya_unavailable",
                    json!({"phase":PHASE,"reason":reason}),
                ) {
                    ui.notice(&format!("Laya outcome was not recorded: {error:#}"));
                }
                ui.laya_activity(
                    status,
                    &format!(
                        "{reason}. Coding remains available; deterministic checks stay active."
                    ),
                );
                None
            }
        }
    }

    pub fn laya_status(&mut self, probe: bool, start: bool, ui: &mut dyn Ui) -> Result<String> {
        let mut text = format!(
            "Laya: {} · advisory decisions\nService: {}\nRequest timeout: {}s\n",
            if self.config.laya.enabled {
                "enabled".to_owned()
            } else {
                format!(
                    "disabled (enable Decision advice in {})",
                    crate::learn::improve::SETTINGS_PATH
                )
            },
            self.config.laya.endpoint,
            self.config.laya.timeout_secs
        );
        if probe || start {
            ui.laya_activity(
                if start { "loading" } else { "checking" },
                if start {
                    "Starting or waiting for Dial's managed service · Esc cancels waiting"
                } else {
                    "Checking the existing service; no prediction or model loading"
                },
            );
            let config = self.config.laya_service();
            let cancel = self.interrupt.clone();
            let health = thread::scope(|scope| -> Result<_> {
                let (tx, rx) = mpsc::sync_channel(1);
                scope.spawn(move || {
                    let _ = tx.send(if start {
                        crate::agent::model::laya::ensure_service(&config, cancel, true)
                    } else {
                        crate::agent::model::laya::health(&config, cancel)
                    });
                });
                receive_with_heartbeat(&rx, ui)?
            });
            match health {
                Ok(health)=>text.push_str(&format!("Live service: {}\nCheckpoint: {}\nDevice: {}\n",health.status,health.checkpoint,health.device)),
                Err(error)=>text.push_str(&format!("Live service unavailable: {error:#}\nUse /laya start, or install the independent runtime with dial laya install --checkpoint PATH.\n")),
            }
        }
        let latest = &self.session.control_state()["laya"];
        if !latest.is_null() {
            text.push_str(&format!(
                "Last session observation (not a live probe): {}\n",
                serde_json::to_string_pretty(latest)?
            ));
        }
        text.push_str("Laya does not write architecture summaries or certify behavior. /laya check probes the service; /understanding architecture inspects memory.\n");
        Ok(text)
    }

    pub(in crate::agent) fn codebase_context(
        &mut self,
        query: &str,
        ui: &mut dyn Ui,
    ) -> Option<String> {
        let cache = self.codebase.as_ref()?.clone();
        let workspace = self.workspace.clone();
        let cancel = self.interrupt.clone();
        // Relevance is a correctness boundary, not an optional efficiency setting.
        // Explicit recall retains its broader index view and separate budget.
        let max_bytes =
            crate::agent::workflow::efficiency::byte_limit(self.config.codebase.context_max_bytes)
                .min(if self.config.efficiency.enabled {
                    crate::agent::workflow::efficiency::byte_limit(
                        self.config.efficiency.ambient_context_bytes,
                    )
                } else {
                    usize::MAX
                });
        let result = thread::scope(|scope| -> Result<_> {
            let (tx, rx) = mpsc::sync_channel(1);
            let worker = scope.spawn(move || {
                let result = cache.refresh(&workspace, &cancel).and_then(|stats| {
                    let architecture = cache.task_architecture_context(query, max_bytes / 2)?;
                    let remaining = max_bytes.saturating_sub(architecture.len());
                    let context = if remaining == 0 {
                        String::new()
                    } else {
                        cache.task_context(query, remaining)?
                    };
                    Ok((stats, format!("{architecture}{context}")))
                });
                let _ = tx.send(result);
            });
            let result = receive_with_heartbeat(&rx, ui);
            if worker.join().is_err() {
                bail!("codebase context worker panicked");
            }
            result?
        });
        match result {
            Ok((stats, context)) => {
                if stats.indexed > 0 || stats.removed > 0 {
                    ui.diagnostic(&format!("Project understanding: indexed {} changed/new sources, reused {}, removed {}{}.",
                        stats.indexed, stats.reused, stats.removed, if stats.partial { "; scan coverage is partial" } else { "" }));
                }
                Some(context)
            }
            Err(_) if self.interrupt.load(Ordering::SeqCst) => None,
            Err(error) => {
                ui.diagnostic(&format!("saved codebase context unavailable: {error:#}"));
                None
            }
        }
    }

    /// Bounded maintenance of project knowledge using the already-selected backend.
    /// It has no tools, cannot edit files, and never becomes a verified training example.
    pub fn synthesize_architecture(&mut self, force: bool, ui: &mut dyn Ui) -> Result<()> {
        if let Some(backend) = &self.backend {
            backend.require_session_identity()?;
        }
        let cache = self
            .codebase
            .clone()
            .context("codebase memory is disabled")?;
        let role_config = self.auth_config.architecture_config();
        let separate =
            Some(role_config.selection()).filter(|selection| *selection != self.config.selection());
        let context_window = if separate.is_some() {
            role_config.context_window
        } else {
            self.context_window()
        };
        ensure!(
            context_window >= 4096,
            "architecture synthesis needs a model context window of at least 4096 tokens"
        );
        let workspace = self.workspace.clone();
        let cancel = self.interrupt.clone();
        let budget = (context_window as usize).clamp(4096, 24000);
        let synthesis = thread::scope(|scope| -> Result<_> {
            let (tx, rx) = mpsc::sync_channel(1);
            let cache = cache.clone();
            scope.spawn(move || {
                let _ = tx.send(
                    cache
                        .refresh(&workspace, &cancel)
                        .and_then(|_| cache.architecture_synthesis(budget, force)),
                );
            });
            receive_with_heartbeat(&rx, ui)?
        })?;
        let Some(synthesis) = synthesis else {
            if force {
                ui.notice(
                    "Nothing to map yet: add source files, then run /understanding build again.",
                );
            }
            return Ok(());
        };
        if self.interrupt.load(Ordering::SeqCst) {
            return Ok(());
        }
        let timeout_seconds = 180u64;
        let timeout_notice = format!(
            "Updating saved architecture from bounded source evidence (one model call, up to {timeout_seconds} seconds; Esc cancels)."
        );
        ui.notice(&timeout_notice);
        let request = Chat {
            messages: vec![
                json!({"role":"system","content":crate::tools::codebase::architecture::SYSTEM}),
                json!({"role":"user","content":synthesis.prompt}),
            ],
            tools: vec![],
            temperature: 0.0,
            max_tokens: (context_window / 3).clamp(512, 2048),
            seed: None,
        };
        ensure!(self.backend.is_some(), "backend is already in use");
        // Construct the role backend before taking the main one so setup failures leave coding usable.
        let role_backend = if separate.is_some() {
            Some(match self.role_backends.remove("small") {
                Some(backend) => backend,
                None => make_backend(&role_config)?,
            })
        } else {
            None
        };
        // Setup failures must not suppress maintenance for unchanged sources after the user fixes
        // model availability. Mark an attempt only once a backend has been constructed.
        cache.mark_architecture_attempt(&synthesis)?;
        let mut main = self.backend.take();
        let mut backend = if let Some(backend) = role_backend {
            main.as_mut().expect("checked main backend").release();
            backend
        } else {
            main.take().expect("checked main backend")
        };
        if let Some(selection) = &separate {
            ui.role("small", &selection.display_name());
        }
        let adapter = if separate.is_some() {
            None
        } else {
            self.version.adapter.clone()
        };
        let external = self.interrupt.clone();
        let local = Arc::new(AtomicBool::new(false));
        let mut yielded = false;
        let call_started = Instant::now();
        let reply = thread::scope(|scope| {
            let (done, wait) = mpsc::channel::<()>();
            let local_watch = local.clone();
            scope.spawn(move || {
                let start = Instant::now();
                loop {
                    if external.load(Ordering::SeqCst)
                        || start.elapsed() >= Duration::from_secs(timeout_seconds)
                    {
                        local_watch.store(true, Ordering::SeqCst);
                        break;
                    }
                    if !matches!(
                        wait.recv_timeout(Duration::from_millis(40)),
                        Err(RecvTimeoutError::Timeout)
                    ) {
                        break;
                    }
                }
            });
            let yield_cancel = local.clone();
            let reply = chat_with_ui(
                &mut backend,
                adapter.as_deref(),
                &request,
                ChatOptions {
                    timeout: timeout_seconds,
                    render_text: false,
                    context_tokens: None,
                    policy: Some(&self.config.reasoning_guard),
                    recovery: Some(&self.config.request_recovery),
                },
                local,
                ui,
                &mut |ui| {
                    if !force && !ui.maintenance_ready() {
                        yielded = true;
                        yield_cancel.store(true, Ordering::SeqCst);
                    }
                },
            );
            drop(done);
            reply
        });
        self.record_aux_request(&backend, &reply, call_started);
        if let Ok(reply) = &reply
            && let Some(cost) = backend.reply_cost(reply)
        {
            self.session_cost = Some(self.session_cost.unwrap_or(0.0) + cost);
        }
        if separate.is_some() {
            backend.release();
            self.role_backends.insert("small".into(), backend);
            self.backend = main;
            ui.role("main", &self.effective_config().selection().display_name());
        } else {
            self.backend = Some(backend);
        }
        if yielded {
            cache.defer_architecture_attempt(&synthesis)?;
            ui.diagnostic("Architecture maintenance yielded to queued work.");
            return Ok(());
        }
        let reply = match reply {
            Ok(reply) => reply,
            Err(error) => {
                cache.defer_architecture_attempt(&synthesis)?;
                return Err(error);
            }
        };
        let save_result = (|| -> Result<usize> {
            self.record(
                0,
                "architecture_synthesis",
                json!({"completion_tokens":reply.completion_tokens,"prompt_tokens":reply.prompt_tokens}),
            )?;
            ensure!(
                reply.tool_calls.is_empty(),
                "architecture synthesis unexpectedly requested tools"
            );
            let model = role_config.model.clone();
            thread::scope(|scope| -> Result<_> {
                let (tx, rx) = mpsc::sync_channel(1);
                let save_cache = cache.clone();
                let save_synthesis = &synthesis;
                scope.spawn(move || {
                    let _ =
                        tx.send(save_cache.save_architecture(save_synthesis, &reply.text, &model));
                });
                receive_with_heartbeat(&rx, ui)?
            })
        })();
        let saved = defer_architecture_attempt_on_failure(&cache, &synthesis, save_result)?;
        ui.event(&crate::agent::ui::ui_event::UiEvent::new(
            crate::agent::ui::ui_event::EventKind::Learning,
            format!("Saved {saved} architecture sections. Inspect /understanding architecture."),
        ));
        Ok(())
    }
}

#[cfg(test)]
mod architecture_retry_tests;
