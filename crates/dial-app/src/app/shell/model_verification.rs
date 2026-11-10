//! Explicit, bounded model-response diagnostics; never invoked by catalog discovery.

use super::{PAINT_EVERY, Shell};
use crate::agent::{Ui, make_backend};
use crate::app::bridge::Bridge;
use crate::app::core::App;
use crate::config::Config;
use anyhow::{Result, ensure};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub(in crate::app) mod coding;

#[derive(Clone, Copy)]
enum Diagnostic {
    Text,
    Coding,
}

impl Diagnostic {
    fn title(self) -> &'static str {
        match self {
            Self::Text => "Verify selected model",
            Self::Coding => "Check coding/tool compatibility",
        }
    }
}
pub(in crate::app) fn identity(config: &Config) -> Option<u64> {
    // Keep credentials in memory only; neither the serialized config nor its hash is displayed.
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_vec(config).ok()?.hash(&mut hash);
    Some(hash.finish())
}

pub(in crate::app) fn failure_hint(error: &anyhow::Error) -> &'static str {
    // Classify without copying server bodies, URLs, headers or credentials to the transcript.
    let text = format!("{error:#}").to_ascii_lowercase();
    // Ports and identifiers containing these digits are not HTTP status codes.
    let has_status = |status| {
        text.split(|c: char| !c.is_ascii_digit())
            .any(|part| part == status)
    };
    if text.contains("auth")
        || text.contains("credential")
        || has_status("401")
        || has_status("403")
    {
        "Authentication unavailable or rejected. Open Providers to repair sign-in or the API key."
    } else if text.contains("connect") || text.contains("refused") || text.contains("dns") {
        "Endpoint unavailable. Check the provider URL and start the local server, or check network access."
    } else if text.contains("model") || has_status("404") {
        "Model unavailable or not configured. Open Models to choose an installed or provider-supported model."
    } else if text.contains("timeout") || text.contains("timed out") {
        "The model did not respond within the diagnostic budget. Check its running state and try again."
    } else {
        "The backend could not complete the diagnostic. Review the selected model, endpoint and authentication in Providers."
    }
}

fn response_test(config: &Config, cancel: Arc<AtomicBool>, budget: Duration) -> Result<()> {
    ensure!(!cancel.load(Ordering::SeqCst), "verification cancelled");
    let mut config = config.clone();
    config.retry.max_attempts = 1;
    config.retry.rotate_accounts = false;
    config.retry.wait_for_quota_reset = false;
    config.retry.fallback_chains.clear();
    config.worker_timeout_secs = 20;
    let request = crate::llm::Chat {
        messages: vec![serde_json::json!({"role":"user","content":"Reply with OK."})],
        tools: vec![],
        temperature: 0.,
        max_tokens: 32,
        seed: None,
    };
    let timed_out = Arc::new(AtomicBool::new(false));
    let result = std::thread::scope(|scope| {
        let (done, rx) = std::sync::mpsc::channel::<()>();
        let flag = cancel.clone();
        let deadline = timed_out.clone();
        scope.spawn(move || {
            if matches!(
                rx.recv_timeout(budget),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ) {
                deadline.store(true, Ordering::SeqCst);
                flag.store(true, Ordering::SeqCst);
            }
        });
        let result = (|| {
            let mut backend = make_backend(&config)?
                .without_steering()
                .with_retry(config.retry.clone());
            let mut profile = backend.model_profile().cloned().unwrap_or_else(|| {
                crate::llm::profiles::ModelProfile::minimal(
                    &config.selection().provider,
                    &config.model,
                )
            });
            profile.max_tokens = Some(profile.max_tokens.filter(|n| *n > 0).unwrap_or(32).min(32));
            profile.inherit_output_limit = false;
            backend = backend.with_profile(profile);
            ensure!(!cancel.load(Ordering::SeqCst), "verification cancelled");
            backend.chat_with_control(None, &request, 20, None, &mut |_| {}, cancel.clone())
        })();
        drop(done);
        result
    });
    ensure!(!timed_out.load(Ordering::SeqCst), "verification timed out");
    ensure!(!cancel.load(Ordering::SeqCst), "verification cancelled");
    ensure!(
        !result?.text.trim().is_empty(),
        "model returned no text response"
    );
    Ok(())
}

impl Shell {
    pub(in crate::app) fn model_response_status(&self, config: &Config) -> &'static str {
        match identity(config)
            .and_then(|key| self.model_verification.filter(|(tested, _)| *tested == key))
        {
            Some((_, true)) => "Verified · model returned text in this session",
            Some((_, false)) => "Not verified · last response test failed or was cancelled",
            None => "Unknown · no response test for this configuration",
        }
    }

    pub(in crate::app) fn verify_selected_model(
        &mut self,
        app: &App,
        agent: Option<&mut crate::agent::Agent<'_>>,
    ) -> Result<()> {
        self.verify_model(app, agent, Diagnostic::Text)
    }

    pub(in crate::app) fn verify_coding_model(
        &mut self,
        app: &App,
        agent: Option<&mut crate::agent::Agent<'_>>,
    ) -> Result<()> {
        self.verify_model(app, agent, Diagnostic::Coding)
    }

    pub(in crate::app) fn model_coding_status(&self, config: &Config) -> &'static str {
        identity(config)
            .and_then(|key| {
                self.coding_verification
                    .filter(|(tested, _)| *tested == key)
            })
            .map_or(
                "Unknown · no coding/tool diagnostic for this configuration",
                |(_, outcome)| {
                    outcome.map_or(
                        "Not verified · failed or cancelled",
                        coding::Outcome::description,
                    )
                },
            )
    }

    fn verify_model(
        &mut self,
        app: &App,
        agent: Option<&mut crate::agent::Agent<'_>>,
        diagnostic: Diagnostic,
    ) -> Result<()> {
        if self.status.busy() {
            self.note(diagnostic.title(), "Wait for the current turn to finish, then explicitly request verification again. No request was queued.");
            return Ok(());
        }
        let config = app.reload()?;
        let Some(key) = identity(&config) else {
            self.note(
                diagnostic.title(),
                "Cannot identify this configuration safely; no request sent.",
            );
            return Ok(());
        };
        if config.uses_local_compute()
            && (!config.compute_locks || (agent.is_none() && app.compute.borrow().is_some()))
        {
            self.note(diagnostic.title(), "Managed-local verification needs an idle agent and compute_locks enabled. No request sent; retry from /doctor when the session is idle.");
            return Ok(());
        }
        let disclosure = match diagnostic {
            Diagnostic::Text => {
                "Send one tiny ‘Reply with OK’ request to the selected model (up to 32 output tokens, 20-second budget). Hosted endpoints use the network and may charge your account; local models may load weights and use compute. No tools or conversation history are sent. Interrupt cancels after starting. This verifies a text response, not tool support or model quality."
            }
            Diagnostic::Coding => {
                "Send up to two synthetic diagnostic requests (up to 128 output tokens each, 20-second overall budget). Hosted endpoints use the network and may charge your account; local models may load weights and use compute. Only a side-effect-free echo tool schema and synthetic messages are sent, never conversation history or project files. No real tools execute. Interrupt cancels. This tests invocation and result continuation, not coding quality or every tool."
            }
        };
        if self.ask(
            diagnostic.title(),
            disclosure,
            &[
                ("Cancel — send nothing", 'c'),
                ("Send verification request", 'v'),
            ],
        ) != Some(1)
        {
            return Ok(());
        }
        if config.uses_local_compute()
            && agent.is_some_and(|agent| !app.release_idle_compute(agent))
        {
            self.note(diagnostic.title(), "A child agent still owns local compute. Wait for it to finish, then retry. No request sent.");
            return Ok(());
        }
        let _lease = if config.uses_local_compute() {
            let lock = crate::learn::auto::lock_file(&crate::config::paths::compute_lock_file(
                &app.layout.user_dir,
            ))?;
            match std::fs::File::try_lock(&lock).map_err(std::io::Error::from) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    self.note(diagnostic.title(), "Local compute is held by another session or worker. Wait for it to release compute, then explicitly retry. No request sent.");
                    return Ok(());
                }
                Err(error) => return Err(error.into()),
            }
            Some(crate::app::discovery::ProbeLease(lock))
        } else {
            None
        };
        match diagnostic {
            Diagnostic::Text => self.model_verification = Some((key, false)),
            Diagnostic::Coding => self.coding_verification = Some((key, None)),
        }
        app.interrupt.store(false, Ordering::SeqCst);
        let cancel = Arc::new(AtomicBool::new(false));
        let prior_phase = self.status.phase;
        self.status.phase = crate::ui::status::Phase::Working;
        self.status.activity = diagnostic.title().into();
        self.composer.set_busy(true);
        let started = Instant::now();
        self.note(
            diagnostic.title(),
            match diagnostic { Diagnostic::Text => "Testing selected configuration · interrupt cancels. No tools or history are sent.", Diagnostic::Coding => "Testing synthetic tool invocation and result continuation · interrupt cancels. No real tools execute." },
        );
        let result = std::thread::scope(|scope| {
            let (tx, rx) = std::sync::mpsc::channel();
            let flag = cancel.clone();
            scope.spawn(move || {
                let result = match diagnostic {
                    Diagnostic::Text => {
                        response_test(&config, flag, Duration::from_secs(20)).map(|()| None)
                    }
                    Diagnostic::Coding => {
                        coding::test(&config, flag, Duration::from_secs(20)).map(Some)
                    }
                };
                let _ = tx.send(result);
            });
            let mut bridge = Bridge {
                app,
                shell: self,
                interrupt: app.interrupt.clone(),
                started,
                last_paint: Instant::now() - PAINT_EVERY,
                failed: false,
                pending: Vec::new(),
                next_id: 0,
            };
            loop {
                bridge.heartbeat();
                if app.interrupt.load(Ordering::SeqCst) {
                    cancel.store(true, Ordering::SeqCst);
                }
                match rx.recv_timeout(Duration::from_millis(80)) {
                    Ok(result) => break result,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(_) => break Err(anyhow::anyhow!("diagnostic worker ended")),
                }
            }
        });
        self.composer.set_busy(false);
        self.status.phase = prior_phase;
        self.status.activity.clear();
        let cancelled = app.interrupt.swap(false, Ordering::SeqCst);
        let current = app.reload()?;
        let unchanged = Some(key) == identity(&current);
        if !unchanged {
            match diagnostic {
                Diagnostic::Text => self.model_verification = None,
                Diagnostic::Coding => self.coding_verification = None,
            }
            self.note(diagnostic.title(), "Selection or configuration changed; this result does not verify the new configuration.");
        } else if cancelled {
            self.note(
                diagnostic.title(),
                "Cancelled · no inference evidence recorded.",
            );
        } else {
            match result {
                Ok(outcome) => {
                    if let Some(outcome) = outcome {
                        self.coding_verification = Some((key, Some(outcome)));
                        self.note(diagnostic.title(), format!("{}. Session-only evidence for this exact configuration; not a coding-quality guarantee.", outcome.description()));
                    } else {
                        self.model_verification = Some((key, true));
                        self.note("Verify selected model", "Verified · selected model returned text. This is session-only evidence for the exact tested configuration, not a tool-support or quality guarantee.");
                    }
                }
                Err(error) => self.note(diagnostic.title(), failure_hint(&error)),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
