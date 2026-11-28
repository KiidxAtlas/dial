//! Laya advice, codebase context recall and architecture synthesis.

use crate::agent::*;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

#[derive(Default)]
pub(in crate::agent) struct GuardrailState {
    history_generation: u64,
    shown_invariants: BTreeSet<String>,
    shown_dead_ends: BTreeSet<String>,
}

impl GuardrailState {
    pub(in crate::agent) fn sync_generation(&mut self, generation: u64) {
        if self.history_generation != generation {
            self.history_generation = generation;
            self.shown_invariants.clear();
            self.shown_dead_ends.clear();
        }
    }
}

fn relative_workspace_path(workspace: &Workspace, raw: &str) -> Option<String> {
    let mut candidate = raw.trim();
    if candidate.is_empty() {
        return None;
    }
    loop {
        if let Ok(resolved) = workspace.resolve(candidate)
            && let Ok(rel) = resolved.strip_prefix(workspace.root())
        {
            let rel = rel.to_string_lossy().into_owned();
            if !rel.is_empty() {
                return Some(rel);
            }
        }
        candidate = candidate.rsplit_once(':')?.0;
    }
}

fn extract_mutation_paths(
    workspace: &Workspace,
    call: &ToolCall,
    output: &ToolOutput,
) -> Vec<String> {
    let mut paths = Vec::new();
    let mut push = |candidate: &str| {
        if let Some(rel) = relative_workspace_path(workspace, candidate)
            && !paths.contains(&rel)
        {
            paths.push(rel);
        }
    };
    if let Some(path) = call.arguments["path"].as_str() {
        push(path);
    }
    if let Some(files) = call.arguments["files"].as_array() {
        for entry in files {
            if let Some(path) = entry["path"].as_str() {
                push(path);
            }
        }
    }
    if call.name == "edit"
        && let Some(input) = call.arguments["input"].as_str()
    {
        for line in input.lines().filter(|line| line.starts_with('[')) {
            if let Some((path, _)) = line.trim_start_matches('[').rsplit_once('#') {
                push(path);
            }
        }
    }
    for changed in &output.execution.changed_paths {
        if let Ok(rel) = changed.strip_prefix(workspace.root()) {
            let rel = rel.to_string_lossy().into_owned();
            if !rel.is_empty() && !paths.contains(&rel) {
                paths.push(rel);
            }
        }
    }
    paths
}

struct GuardrailEvidence<'a> {
    check_failed: bool,
    attached_verify_failed: Option<&'a str>,
    changed_files: &'a [String],
    baselines: &'a BTreeMap<String, String>,
}

fn attach_guardrails_with(
    cache: &Codebase,
    workspace: &Workspace,
    guardrails: &mut GuardrailState,
    history_generation: u64,
    call: &ToolCall,
    output: &mut ToolOutput,
    evidence: GuardrailEvidence<'_>,
) -> Vec<String> {
    let GuardrailEvidence {
        check_failed,
        attached_verify_failed,
        changed_files,
        baselines,
    } = evidence;
    let mut diagnostics = Vec::new();
    guardrails.sync_generation(history_generation);
    let mut effective = call.clone();
    let _ = crate::agent::window::file_handles::expand(workspace, &mut effective);

    // 1) Path-scoped 1-line invariants: lazily attached once per generation on file touch.
    let invariant_paths = if effective.name == "read" && !output.is_error {
        crate::agent::verify::caller_obligations::read_targets(workspace, &effective.arguments)
    } else if matches!(effective.name.as_str(), "edit" | "write" | "ast_edit") {
        extract_mutation_paths(workspace, &effective, output)
    } else {
        Vec::new()
    };
    if !invariant_paths.is_empty() {
        let invariant_baselines = if effective.name == "read" {
            BTreeMap::new()
        } else {
            baselines.clone()
        };
        match cache.invariants_for_paths(
            &invariant_paths,
            &invariant_baselines,
            &mut guardrails.shown_invariants,
            crate::tools::knowledge::codebase::guardrails::DEFAULT_ATTACH_BYTES,
        ) {
            Ok(Some(note)) => {
                output.text.push('\n');
                output.text.push_str(&note);
            }
            Ok(None) => {}
            Err(error) => diagnostics.push(format!("codebase invariants unavailable: {error:#}")),
        }
    }

    // 2) Failure-triggered 1-line dead ends: zero tokens on passing runs.
    let edit_failed =
        matches!(effective.name.as_str(), "edit" | "write" | "ast_edit") && output.is_error;
    let verify_failed = effective.name == "verify" && output.is_error;
    let bash_failed = effective.name == "bash"
        && (output.is_error
            || check_failed
            || output.process_outcome().is_some_and(|process| {
                process.started
                    && (process.exit_code.is_some_and(|code| code != 0)
                        || process.timed_out
                        || process.signal.is_some())
            }));
    if edit_failed || verify_failed || bash_failed || attached_verify_failed.is_some() {
        let mut failure_paths = extract_mutation_paths(workspace, &effective, output);
        for path in changed_files {
            if !failure_paths.contains(path) {
                failure_paths.push(path.clone());
            }
        }
        let failure_text = if !output.is_error
            && let Some(text) = attached_verify_failed
        {
            text.to_owned()
        } else {
            output.text.clone()
        };
        match cache.dead_ends_for_failure(
            &failure_paths,
            &failure_text,
            baselines,
            &mut guardrails.shown_dead_ends,
            crate::tools::knowledge::codebase::guardrails::DEFAULT_ATTACH_BYTES,
        ) {
            Ok(Some(note)) => {
                output.text.push('\n');
                output.text.push_str(&note);
            }
            Ok(None) => {}
            Err(error) => {
                diagnostics.push(format!("codebase dead-end lookup unavailable: {error:#}"))
            }
        }
    }
    diagnostics
}

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
        let now = crate::runtime::clock::unix_secs();
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
                let result = cache
                    .refresh(&workspace, &cancel)
                    .and_then(|stats| Ok((stats, cache.task_brief(query, max_bytes, true)?)));
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
            ui.role("small", &selection.display_label());
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
            ui.role("main", &self.effective_config().selection().display_label());
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

    /// Lazily attach path-scoped 1-line invariants on file touch and failure-triggered 1-line
    /// dead ends when an edit, verify or bash check fails.
    pub(in crate::agent) fn attach_codebase_guardrails(
        &mut self,
        call: &ToolCall,
        output: &mut ToolOutput,
        check_failed: bool,
        attached_verify_failed: Option<&str>,
        observed_sources: &BTreeMap<String, Option<String>>,
        ui: &mut dyn Ui,
    ) {
        if self.hermetic {
            return;
        }
        let Some(cache) = self.codebase.clone() else {
            return;
        };
        let is_read = call.name == "read" && !output.is_error;
        let is_mutation = matches!(call.name.as_str(), "edit" | "write" | "ast_edit");
        let is_failure = (is_mutation && output.is_error)
            || (call.name == "verify" && output.is_error)
            || (call.name == "bash"
                && (output.is_error
                    || check_failed
                    || output.process_outcome().is_some_and(|process| {
                        process.started
                            && (process.exit_code.is_some_and(|code| code != 0)
                                || process.timed_out
                                || process.signal.is_some())
                    })))
            || attached_verify_failed.is_some();
        if !is_read && !is_mutation && !is_failure {
            return;
        }
        let mut changed_files = Vec::new();
        let mut baselines = BTreeMap::new();
        if is_mutation || is_failure {
            for (path, hash) in observed_sources {
                if let Some(digest) = hash {
                    baselines.insert(path.clone(), digest.clone());
                }
            }
            if let (Some(turn), Some(checkpoints)) =
                (self.step_checks.turn, self.checkpoints.as_ref())
            {
                changed_files = checkpoints.changed_files(turn);
                if let Ok(versions) = checkpoints.turn_versions(turn) {
                    for version in versions {
                        if version.in_project
                            && let Some(before) = &version.before
                        {
                            baselines
                                .entry(version.path)
                                .or_insert_with(|| format!("{:x}", Sha256::digest(before)));
                        }
                    }
                }
            }
        }
        let generation = self.session.context_history_generation();
        let diagnostics = attach_guardrails_with(
            &cache,
            &self.workspace,
            &mut self.guardrails,
            generation,
            call,
            output,
            GuardrailEvidence {
                check_failed,
                attached_verify_failed,
                changed_files: &changed_files,
                baselines: &baselines,
            },
        );
        for diagnostic in diagnostics {
            ui.diagnostic(&diagnostic);
        }
    }
}

#[cfg(test)]
mod architecture_retry_tests;
#[cfg(test)]
mod guardrail_tests;
