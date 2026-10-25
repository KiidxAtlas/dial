//! Active model route: the live backend, the configuration that resolved it (auth identity and
//! context planning), its loaded-window cap, and any temporary fallback or context promotion. These
//! change together, so a fallback is never retained without its cooldown and the configured primary
//! stays restorable. A promotion has no cooldown: it holds until the user switches models.
use crate::agent::run::turn::join_turn_usage;
use crate::agent::*;

/// A temporary fallback installed in place of the configured primary.
struct Fallback {
    /// The primary returns at the first request boundary after this instant.
    until: Instant,
    /// The primary's loaded-window cap, restored together with the primary configuration.
    primary_cap: Option<u32>,
}

/// A larger-window model installed in place of the configured primary by context promotion. It
/// becomes the route's primary: fallbacks fail over from it and return to it, never to the model it
/// replaced, until the user switches models (`reset`).
struct Promoted {
    /// The configuration that resolved the promoted backend.
    config: Config,
}

/// Session-lifetime owner of the active model route. Its state lives as long as the backend it
/// describes: a turn end, verification or stop-hook reentry neither drops a retained fallback nor
/// forgets its cooldown.
#[derive(Default)]
pub(in crate::agent) struct ActiveRoute {
    /// Fallback candidates tried in the current failure episode, as an index into the primary's list.
    tried: usize,
    fallback: Option<Fallback>,
    promoted: Option<Promoted>,
}

impl ActiveRoute {
    /// Forgets the route. Used when the primary itself is replaced (model switch, session change).
    pub(in crate::agent) fn reset(&mut self) {
        *self = Self::default();
    }

    /// Whether a temporary fallback is installed in place of the primary.
    pub(in crate::agent) fn on_fallback(&self) -> bool {
        self.fallback.is_some()
    }

    /// Whether an installed fallback is still inside its cooldown at `now`.
    pub(in crate::agent) fn cooling_at(&self, now: Instant) -> bool {
        self.fallback
            .as_ref()
            .is_some_and(|fallback| now < fallback.until)
    }

    /// Whether the primary must return at `now`: a fallback is installed and its cooldown has passed.
    pub(in crate::agent) fn restore_due_at(&self, now: Instant) -> bool {
        self.on_fallback() && !self.cooling_at(now)
    }
}

impl Agent<'_> {
    /// Installs `config` as the live route in one step. The backend is built and bound to this
    /// session and to the turn's usage scope before anything is replaced, so a failed construction
    /// leaves the previous route untouched. The resolved configuration, loaded-window cap and
    /// per-route caches change with the backend.
    pub(in crate::agent) fn install_route(
        &mut self,
        backend: &mut Backend,
        config: &Config,
        context_cap: Option<u32>,
    ) -> Result<()> {
        let mut next = make_backend(config)?.with_session_id(self.session.id().to_owned());
        join_turn_usage(&self.usage_scope, &mut next);
        let resolved = session_auth_config(config, &next, &mut self.session)?;
        *backend = next;
        self.auth_config = resolved;
        self.context_cap = context_cap;
        self.role_backends.clear();
        self.subagent_backend = None;
        Ok(())
    }

    /// Restores the configured primary once a fallback's cooldown has elapsed. Runs before the
    /// request is prepared, so compaction and the request are sized for the route that answers it.
    /// Returns whether the route changed.
    pub(in crate::agent) fn restore_primary_if_due(
        &mut self,
        backend: &mut Backend,
        ui: &mut dyn Ui,
    ) -> Result<bool> {
        if !self.route.restore_due_at(Instant::now()) {
            return Ok(false);
        }
        let primary_cap = self.route.fallback.as_ref().and_then(|f| f.primary_cap);
        let primary = self.primary_config();
        self.install_route(backend, &primary, primary_cap)?;
        self.route.fallback = None;
        self.route.tried = 0;
        self.request_phase
            .recover("fallback cooldown elapsed; returning to the primary model");
        ui.diagnostic("Fallback cooldown elapsed; returning to the primary model");
        Ok(true)
    }

    /// The configuration a fallback fails over from and returns to: the promoted model when context
    /// promotion replaced the configured one, else the session's effective configuration.
    fn primary_config(&self) -> Config {
        match &self.route.promoted {
            Some(promoted) => promoted.config.clone(),
            None => self.effective_config().clone(),
        }
    }

    /// Switches to a configured larger-window model instead of compacting. Applies only when
    /// `[compaction.promotion]` is enabled and the session is not hermetic. The first target (see
    /// `fallback::promotion_targets`) that differs from the live model, has a strictly larger
    /// window, can be constructed, and holds `request_tokens` plus the output reserve `output_tokens`
    /// is installed; an `overflow` retry judges the hard window only, while a threshold promotion
    /// must also raise the history budget (the economy cap can hold it level). Replaces any active
    /// fallback and never returns to the earlier model on a timer: only a user model switch resets
    /// it. Returns whether the route changed; false leaves it untouched for compaction to run.
    pub(in crate::agent) fn try_promote_context(
        &mut self,
        backend: &mut Backend,
        request_tokens: u64,
        output_tokens: u32,
        overflow: bool,
        ui: &mut dyn Ui,
    ) -> bool {
        let policy = &self.config.compaction;
        if self.hermetic || !policy.enabled || !policy.promotion.enabled {
            return false;
        }
        let live = self.auth_config.selection();
        let current = self.context_window();
        let economy = self.config.efficiency.enabled && !self.hermetic;
        let budget = self.budget_window();
        let base = self.primary_config();
        let targets = crate::llm::fallback::promotion_targets(&base, "default", &live);
        for target in &targets {
            let next_config = base.with_selection(target);
            let window = next_config.context_window;
            if window <= current
                || (!overflow
                    && crate::agent::base::helpers::capped_window(
                        window,
                        self.config.efficiency.history_token_cap,
                        economy,
                    ) <= budget)
                || request_tokens
                    > crate::agent::compact::compaction::prompt_budget(
                        &self.config.compaction,
                        window,
                        output_tokens,
                    )
            {
                continue;
            }
            if let Err(build_error) = self.install_route(backend, &next_config, None) {
                ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                    crate::agent::ui::ui_event::EventKind::Model,
                    format!(
                        "Promotion target {}/{} could not be constructed ({build_error:#}); trying the next target",
                        target.provider, target.model
                    ),
                ));
                continue;
            }
            self.route.fallback = None;
            self.route.tried = 0;
            self.route.promoted = Some(Promoted {
                config: next_config,
            });
            self.compaction_runtime.cancel();
            self.request_phase
                .recover("switching to a larger-window model");
            ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                crate::agent::ui::ui_event::EventKind::Model,
                format!(
                    "Context needs about {request_tokens} tokens; promoting from {}/{} ({current} tokens) to {}/{} ({window} tokens) instead of compacting",
                    live.provider, live.model, target.provider, target.model
                ),
            ));
            return true;
        }
        false
    }

    /// Moves to the next configured fallback after a failure that `should_fallback` accepts.
    /// Returns false when no remaining candidate can be constructed; the route is then unchanged.
    pub(in crate::agent) fn fail_over(
        &mut self,
        backend: &mut Backend,
        error: &anyhow::Error,
        ui: &mut dyn Ui,
    ) -> bool {
        let primary = self.primary_config();
        let selected = primary.selection();
        let fallbacks = crate::llm::fallback::fallback_models(&primary, "default", &selected);
        // A failure on the primary starts a new episode; a failure on a fallback continues the chain.
        let primary_cap = match &self.route.fallback {
            Some(active) => active.primary_cap,
            None => {
                self.route.tried = 0;
                self.context_cap
            }
        };
        let cooldown = error
            .downcast_ref::<llm::HttpError>()
            .and_then(|http| http.retry_after)
            .unwrap_or(Duration::from_secs(5 * 60));
        while let Some(next) = fallbacks.get(self.route.tried) {
            self.route.tried += 1;
            let next_config = primary.with_selection(next);
            if let Err(build_error) = self.install_route(backend, &next_config, None) {
                ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                    crate::agent::ui::ui_event::EventKind::Model,
                    format!(
                        "Configured fallback {}/{} could not be constructed ({build_error:#}); trying the next fallback",
                        next.provider, next.model
                    ),
                ));
                continue;
            }
            self.route.fallback = Some(Fallback {
                until: Instant::now() + cooldown,
                primary_cap,
            });
            self.request_phase
                .recover("switching to a configured fallback model");
            ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                crate::agent::ui::ui_event::EventKind::Model,
                format!(
                    "Primary model failed; continuing with configured fallback {}/{}",
                    next.provider, next.model
                ),
            ));
            return true;
        }
        false
    }

    /// Says so when a fallback retained from an earlier request or turn is still the live route.
    pub(in crate::agent) fn report_retained_fallback(&self, ui: &mut dyn Ui) {
        if self.route.cooling_at(Instant::now()) {
            let active = self.auth_config.selection();
            ui.diagnostic(&format!(
                "Continuing on fallback {}/{} until its cooldown ends; the configured primary returns afterwards.",
                active.provider, active.model
            ));
        }
    }
}

#[cfg(test)]
mod tests;
