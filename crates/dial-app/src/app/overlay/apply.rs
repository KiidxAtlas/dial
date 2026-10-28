//! Applying overlay events as semantic configuration changes.

use crate::app::overlay::auth::PreparedModel;
use crate::app::overlay::compute::compute_starts;
use crate::app::overlay::env::{OverlayEnv, favorite_routes, settings_tables};
use crate::app::overlay::{ActiveOverlay, start_streaming_job};
use crate::app::selection::{clear_role_changes, provider_changes};
use crate::app::shell::Shell;
use crate::config::selection::changes as selection_changes;
use crate::config::settings_store::{self, Change};
use crate::config::types::Approval;
use crate::llm::catalog;
use crate::ui::overlay::OverlayEvent;
use anyhow::{Result, ensure};
use std::cell::RefCell;
use std::sync::atomic::Ordering;

impl OverlayEnv<'_> {
    /// Returns true when the overlay should close.
    pub(super) fn apply(&mut self, event: OverlayEvent, overlay: &mut ActiveOverlay) -> bool {
        match event {
            OverlayEvent::Close => {
                if self
                    .auth_job
                    .as_ref()
                    .is_some_and(|job| job.model.is_some())
                {
                    self.auth_job.take();
                }
                return true;
            }
            OverlayEvent::None => {}
            OverlayEvent::SetupDep { id } => self.start_setup(overlay, id),
            OverlayEvent::JobInput { name, bytes } => {
                let outcome = if matches!(self.pty_approval, Approval::Readonly) {
                    Err(anyhow::anyhow!(
                        "PTY input is unavailable in read-only approval mode"
                    ))
                } else if bytes.len() > 64 * 1024 {
                    Err(anyhow::anyhow!("interactive PTY input exceeds 64 KiB"))
                } else if let Some(background) = &self.background_control {
                    background
                        .attach_user_pty(&name, &bytes)
                        .map(|written| format!("sent {written} bytes to {name}"))
                        .map_err(anyhow::Error::msg)
                } else {
                    Err(anyhow::anyhow!("background job control is unavailable"))
                };
                self.report(overlay, outcome);
            }
            OverlayEvent::Set { path, value, scope } => {
                *self.scope = scope;
                if path == "model_favorites" {
                    let outcome = self.save(&[Change::Set { path, value }], scope);
                    self.report(overlay, outcome.map(|_| "Favorites saved".to_owned()));
                    if let (ActiveOverlay::Models(models), Some(config)) = (overlay, &self.latest) {
                        models.set_favorites(favorite_routes(config));
                    }
                } else {
                    let mut changes = vec![Change::Set {
                        path: path.clone(),
                        value,
                    }];
                    // The embedding override saves as a pair: add the half the panel held back.
                    let partner = crate::ui::overlay::settings::pair_partner(&path);
                    if let ActiveOverlay::Settings(settings) = &mut *overlay
                        && let Some((staged, staged_value)) = settings.take_staged_pair()
                        && Some(staged.as_str()) == partner
                    {
                        changes.push(Change::Set {
                            path: staged,
                            value: staged_value,
                        });
                    }
                    self.setting(overlay, &path, &changes, scope);
                }
            }
            OverlayEvent::Unset { path, scope } => {
                *self.scope = scope;
                let mut changes = vec![Change::Remove { path: path.clone() }];
                // Clearing one half of the embedding override clears the pair.
                if let Some(partner) = crate::ui::overlay::settings::pair_partner(&path) {
                    changes.push(Change::Remove {
                        path: partner.to_owned(),
                    });
                }
                self.setting(overlay, &path, &changes, scope);
            }
            OverlayEvent::AssignModel {
                role,
                selection,
                scope,
            } => {
                *self.scope = scope;
                let config = self
                    .latest
                    .clone()
                    .unwrap_or_else(|| self.app.config.clone());
                let embedding_role = role == "embedding";
                let outcome = (|| -> Result<String> {
                    let changes = selection_changes(&config, &selection, &role)?;
                    settings_store::preview_model_config(&self.app.layout, scope, &changes, &role)?;
                    let message = if role == "main" {
                        format!("switched to {}", selection.display_name())
                    } else {
                        format!("{role} set to {}", selection.display_name())
                    };
                    // OS models and Laya's typed service do not use chat-weight acquisition.
                    if crate::config::selection::requires_local_preparation(&config, &selection) {
                        let model = selection.model.clone();
                        let python = config.python.clone();
                        let started =
                            start_streaming_job(self.auth_job, "Model", move |cancel, progress| {
                                let progress = RefCell::new(progress);
                                crate::llm::download::ensure_local_model_with(
                                    &python,
                                    &model,
                                    &crate::llm::download::Choice::Auto,
                                    &cancel,
                                    &|line| progress.borrow_mut()(line),
                                )?;
                                ensure!(
                                    !cancel.load(Ordering::SeqCst),
                                    "model selection cancelled"
                                );
                                Ok(message)
                            });
                        ensure!(
                            started,
                            "Another background operation is still running; try again when it finishes."
                        );
                        self.auth_job.as_mut().unwrap().model = Some(PreparedModel {
                            changes,
                            role: Some(role),
                            scope,
                        });
                        Ok("Preparing model in the background; settings save only on success. Esc closes this panel and cancels preparation.".into())
                    } else {
                        ensure!(
                            !self
                                .auth_job
                                .as_ref()
                                .is_some_and(|job| job.model.is_some()),
                            "Cancel the pending model selection first (Esc)."
                        );
                        self.app.save_settings(scope, &changes, Some(&role))?;
                        self.reload_saved()?;
                        Ok(message)
                    }
                })();
                let assigned =
                    outcome.is_ok() && self.auth_job.as_ref().is_none_or(|job| job.model.is_none());
                let outcome = match outcome {
                    Ok(message) if assigned && embedding_role => {
                        Ok(match self.queue_semantic_offer() {
                            Some(note) => format!("{message}. {note}"),
                            None => message,
                        })
                    }
                    other => other,
                };
                self.report(overlay, outcome);
                self.refresh_roles(overlay);
                // The Settings Embedding model row shows the role and may complete a pending switch.
                if assigned
                    && let (ActiveOverlay::Settings(settings), Some(fresh)) =
                        (&mut *overlay, &self.latest)
                    && let Ok((values, global, project)) = settings_tables(self.app, fresh)
                {
                    settings.replace_values(values, project, global);
                    settings.roles_changed();
                }
            }
            OverlayEvent::DownloadSetting { path, value, scope } => {
                *self.scope = scope;
                let config = self
                    .latest
                    .clone()
                    .unwrap_or_else(|| self.app.config.clone());
                let outcome = (|| -> Result<String> {
                    let changes = vec![Change::Set {
                        path: path.clone(),
                        value: toml::Value::String(value.clone()),
                    }];
                    // Refuse an invalid value before spending a download on it.
                    settings_store::preview_config(
                        &self.app.layout,
                        scope,
                        &changes,
                        &self.app.overlay,
                    )?;
                    let message = format!("{path} set to {value}");
                    let model = value.clone();
                    let python = config.python.clone();
                    let started =
                        start_streaming_job(self.auth_job, "Model", move |cancel, progress| {
                            let progress = RefCell::new(progress);
                            crate::llm::download::ensure_local_model_with(
                                &python,
                                &model,
                                &crate::llm::download::Choice::Auto,
                                &cancel,
                                &|line| progress.borrow_mut()(line),
                            )?;
                            ensure!(!cancel.load(Ordering::SeqCst), "download cancelled");
                            Ok(message)
                        });
                    ensure!(
                        started,
                        "Another background operation is still running; try again when it finishes."
                    );
                    self.auth_job.as_mut().unwrap().model = Some(PreparedModel {
                        changes,
                        role: None,
                        scope,
                    });
                    Ok(format!(
                        "Downloading {value} in the background; {path} saves only on success. Esc closes this panel and cancels the download."
                    ))
                })();
                self.report(overlay, outcome);
            }
            OverlayEvent::ClearRole { role, scope } => {
                *self.scope = scope;
                let outcome =
                    clear_role_changes(&role, scope).and_then(|changes| self.save(&changes, scope));
                self.report(
                    overlay,
                    outcome.map(|config| {
                        if config.role_selection(&role).is_some() {
                            format!("{role}: {scope:?} assignment removed, but another scope still assigns a model; Ctrl+S switches scope")
                        } else {
                            format!("{role} assignment cleared")
                        }
                    }),
                );
                self.refresh_roles(overlay);
            }
            OverlayEvent::AddProvider { id, spec, scope } => {
                *self.scope = scope;
                let outcome = self.save(&provider_changes(&id, &spec), scope);
                if let (Ok(config), ActiveOverlay::Providers(providers)) = (&outcome, &mut *overlay)
                {
                    providers.set_configured(config.providers.clone());
                }
                self.report(overlay, outcome.map(|_| format!("provider {id} updated")));
                self.catalog.refresh(self.app);
            }
            OverlayEvent::RemoveProvider { id, scope } => {
                *self.scope = scope;
                let outcome = self.save(
                    &[Change::Remove {
                        path: format!("providers.{id}"),
                    }],
                    scope,
                );
                self.report(overlay, outcome.map(|_| format!("provider {id} removed")));
                self.catalog.refresh(self.app);
            }
            OverlayEvent::SaveCredential {
                provider,
                base_url,
                mut token,
            } => {
                let verified = matches!(provider.as_str(), "openai" | "anthropic");
                let outcome = catalog::verify_and_save_credential(
                    &self.app.layout.user_dir,
                    &provider,
                    &base_url,
                    &mut token,
                );
                let success = if verified {
                    format!("{provider} API key verified and saved (owner-only file)")
                } else {
                    "API key saved (owner-only file)".to_owned()
                };
                self.report(overlay, outcome.map(|_| success));
                self.catalog.refresh(self.app);
            }
            OverlayEvent::OpenRouterLogin => {
                let dir = self.app.layout.user_dir.clone();
                // A streaming job: the sign-in URL is progress, so it reaches the transcript even
                // when no browser opens and the user completes the callback by hand.
                let started =
                    start_streaming_job(self.auth_job, "OpenRouter", move |cancel, progress| {
                        let mut key = crate::llm::auth::openrouter_login(cancel, progress)?;
                        catalog::verify_and_save_credential(
                            &dir,
                            "openrouter",
                            catalog::OPENROUTER_BASE_URL,
                            key.as_mut_string(),
                        )?;
                        Ok("OpenRouter sign-in verified; key saved locally".into())
                    });
                if started {
                    self.report(
                        overlay,
                        Ok("Waiting for OpenRouter sign-in in your browser; sign-in continues if you close this panel".into()),
                    );
                } else {
                    self.report(
                        overlay,
                        Err(anyhow::anyhow!(
                            "An authentication action is already running"
                        )),
                    );
                }
            }
            OverlayEvent::ChatGptLogin { client_id } => {
                let dir = self.app.layout.user_dir.clone();
                self.start_auth(overlay, "ChatGPT", move |cancel| {
                    crate::llm::chatgpt::login(&dir, client_id.as_deref(), cancel)
                });
            }
            OverlayEvent::ChatGptCancel => {
                self.auth_job.take();
                if let ActiveOverlay::Providers(providers) = overlay {
                    providers.set_chatgpt_pending(false);
                }
                self.report(overlay, Ok("Cancelled pending ChatGPT sign-in".into()));
            }
            OverlayEvent::ChatGptSelect { client_id } => {
                let dir = self.app.layout.user_dir.clone();
                self.start_auth(overlay, "ChatGPT", move |_| {
                    crate::llm::chatgpt::select_account(&dir, &client_id).map(|_| {
                        "ChatGPT account selected; choose an available model in /models".into()
                    })
                });
            }
            OverlayEvent::ChatGptSignOut { client_id } => {
                let dir = self.app.layout.user_dir.clone();
                self.start_auth(overlay, "ChatGPT", move |cancel| {
                    crate::llm::chatgpt::sign_out(&dir, &client_id, cancel)
                });
            }
            OverlayEvent::AnthropicLogin => {
                let dir = self.app.layout.user_dir.clone();
                self.start_auth(overlay, "Claude", move |cancel| {
                    crate::llm::anthropic_oauth::login(&dir, cancel)
                });
            }
            OverlayEvent::AnthropicSignOut => {
                // A login still waiting for its callback would re-save tokens after sign-out.
                if self
                    .auth_job
                    .as_ref()
                    .is_some_and(|job| job.label == "Claude")
                {
                    self.auth_job.take();
                }
                let outcome = crate::llm::anthropic_oauth::sign_out(&self.app.layout.user_dir);
                if outcome.is_ok()
                    && let ActiveOverlay::Providers(providers) = &mut *overlay
                {
                    providers.set_anthropic_signed_in(false);
                }
                self.report(overlay, outcome);
                self.catalog.refresh(self.app);
            }
            OverlayEvent::OAuthLogin { id } => match crate::llm::oauth::flow(&id) {
                Some(flow) => {
                    let dir = self.app.layout.user_dir.clone();
                    self.start_auth(overlay, flow.name, move |cancel| {
                        let ui = crate::llm::oauth::BannerInteraction { id: flow.id };
                        crate::llm::oauth::login(&dir, flow.id, &ui, cancel)
                    });
                }
                None => self.report(
                    overlay,
                    Err(anyhow::anyhow!("Unknown sign-in provider '{id}'")),
                ),
            },
            OverlayEvent::OAuthSignOut { id } => match crate::llm::oauth::flow(&id) {
                Some(flow) => {
                    if self
                        .auth_job
                        .as_ref()
                        .is_some_and(|job| job.label == flow.name)
                    {
                        self.auth_job.take();
                    }
                    let outcome = crate::llm::oauth::sign_out(&self.app.layout.user_dir, flow.id);
                    if outcome.is_ok()
                        && let ActiveOverlay::Providers(providers) = &mut *overlay
                    {
                        providers.set_oauth_accounts(crate::llm::oauth::accounts_by_provider(
                            &self.app.layout.user_dir,
                        ));
                    }
                    self.report(overlay, outcome);
                    self.catalog.refresh(self.app);
                }
                None => self.report(
                    overlay,
                    Err(anyhow::anyhow!("Unknown sign-in provider '{id}'")),
                ),
            },
            OverlayEvent::OAuthSelectAccount { id, key } => match crate::llm::oauth::flow(&id) {
                Some(flow) => {
                    let outcome =
                        crate::llm::oauth::select_account(&self.app.layout.user_dir, flow.id, &key);
                    if outcome.is_ok()
                        && let ActiveOverlay::Providers(providers) = &mut *overlay
                    {
                        providers.set_oauth_accounts(crate::llm::oauth::accounts_by_provider(
                            &self.app.layout.user_dir,
                        ));
                    }
                    self.report(overlay, outcome);
                    self.catalog.refresh(self.app);
                }
                None => self.report(
                    overlay,
                    Err(anyhow::anyhow!("Unknown sign-in provider '{id}'")),
                ),
            },
            OverlayEvent::OAuthRemoveAccount { id, key } => match crate::llm::oauth::flow(&id) {
                Some(flow) => {
                    let outcome = crate::llm::oauth::sign_out_account(
                        &self.app.layout.user_dir,
                        flow.id,
                        &key,
                    );
                    if outcome.is_ok()
                        && let ActiveOverlay::Providers(providers) = &mut *overlay
                    {
                        providers.set_oauth_accounts(crate::llm::oauth::accounts_by_provider(
                            &self.app.layout.user_dir,
                        ));
                    }
                    self.report(overlay, outcome);
                    self.catalog.refresh(self.app);
                }
                None => self.report(
                    overlay,
                    Err(anyhow::anyhow!("Unknown sign-in provider '{id}'")),
                ),
            },
            OverlayEvent::RemoveCredential { base_url } => {
                let dir = self.app.layout.user_dir.clone();
                let outcome = catalog::remove_credential(&dir, &base_url)
                    .map(|_| "API key removed; sign-ins are unchanged".to_owned());
                self.report(overlay, outcome);
                self.catalog.refresh(self.app);
            }
            OverlayEvent::Refresh => self.catalog.refresh(self.app),
            OverlayEvent::VerifyLora(selection) => {
                self.verify_lora = Some(selection);
                return true;
            }
            OverlayEvent::PreviewTheme(Some(name)) => {
                let _ = self.themes.preview(&name);
            }
            OverlayEvent::PreviewTheme(None) => self.themes.restore(),
            OverlayEvent::Rebind { action, chords } => {
                let chords: Vec<&str> = chords.iter().map(String::as_str).collect();
                let outcome = self
                    .keys
                    .edit_and_save(&self.app.layout.user_dir, action, &chords);
                if let ActiveOverlay::Shortcuts(shortcuts) = overlay {
                    shortcuts.settled(
                        outcome
                            .map(|_| "Saved for all projects; shortcuts are active now".to_owned())
                            .map_err(|error| format!("{error:#}")),
                    );
                }
            }
            OverlayEvent::Action(action) => {
                if let Some(payload) = action.strip_prefix("extensions:") {
                    Shell::queue_extensions(self.extensions_pending, *self.scope, payload, overlay);
                } else if let Some(payload) = action.strip_prefix("compute:") {
                    if compute_starts(payload)
                        && let Some(release) = self.release_compute.as_mut()
                    {
                        // The training worker needs the compute lease and the resident server's memory.
                        release();
                    }
                    if let Some(monitor) = &self.compute_monitor {
                        match serde_json::from_str(payload)
                            .map_err(anyhow::Error::from)
                            .and_then(|action| {
                                monitor
                                    .commands
                                    .try_send(action)
                                    .map_err(anyhow::Error::from)
                            }) {
                            Ok(()) => overlay.banner("Applying compute control…".into(), false),
                            Err(error) => overlay.banner(error.to_string(), true),
                        }
                    } else {
                        overlay.banner(
                            "Compute control is unavailable here; the compute monitor is not running.".into(),
                            true,
                        );
                    }
                    return false;
                }
                if action.starts_with("subagent-open:") || action.starts_with("subagent-hub") {
                    self.next = Some(action);
                    return true;
                }
                if let ActiveOverlay::SubagentViewer(viewer) = &mut *overlay
                    && action.starts_with("subagent-")
                {
                    match crate::ui::subagent_model::act(&action, self.live_tasks) {
                        Some(Ok(crate::ui::subagent_model::ActionOutcome::Banner(text))) => {
                            viewer.action_succeeded(text);
                        }
                        Some(Ok(crate::ui::subagent_model::ActionOutcome::Resume {
                            id,
                            message,
                        })) => {
                            self.next = Some(format!("subagent-resume:{id}\n{message}"));
                            return true;
                        }
                        Some(Err(error)) => viewer.action_failed(format!("{error:#}")),
                        None => {}
                    }
                    return false;
                }
                if let ActiveOverlay::Subagents(hub) = &mut *overlay
                    && action.starts_with("subagent-")
                {
                    if let Some(action) = hub.act(&action, self.live_tasks) {
                        self.next = Some(action);
                        return true;
                    }
                    return false;
                }
                if action.starts_with("open:")
                    || action.starts_with("command:")
                    || action.starts_with("test:")
                {
                    self.next = Some(action);
                    return true;
                }
            }
        }
        false
    }
}
