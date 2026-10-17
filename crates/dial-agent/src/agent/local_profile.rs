//! Local-model profile (`[assistance.local]`): the one owner of the policies a locally served main
//! model gets on top of `assistance.lean_local`: its tier, the request sampling, a per-request
//! reasoning cap, in-session escalation, request hints (thinking by phase, keep-loaded, the
//! llama.cpp prompt-cache slot, loop penalties), side-model routing, context advice, a time budget
//! from measured speed, and the per-model evidence in `local-profile.json`. The edit grammar default
//! is resolved in `Config::resolved_edit_mode`, so workers on a local route get it as well.
//!
//! Every decision comes from observed facts (the route, the planned window, the server's own
//! reports, failing rounds, measured speed), never from model names. Hosted routes and hermetic runs
//! are unchanged, explicit pins win, and suggestions are never applied without the user.

use super::*;
use crate::config::types::{
    EditMode, Harness, LocalSamplingSource, LocalSideModels, LocalThinking,
};
use crate::llm::local_evidence::{self, Evidence, Mean};
use crate::llm::local_hints::{LocalHints, LocalServer};
use crate::llm::local_server::{self, ServerFacts};
use crate::llm::profiles::ModelProfile;
use std::collections::BTreeSet;

/// How much of the harness a local route sees, from the planned window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tier {
    /// Lean prompt and a fixed minimal tool set up front (`assistance.local.small_window`).
    Small,
    /// Lean prompt and the bounded lean tool budget (`assistance.lean_local`).
    Lean,
    /// The full prompt and tool offer.
    Full,
}

/// Highest escalation step: 1 offers the minimal tool set and lean prompt, 2 also switches an
/// unpinned edit grammar to `replace`.
const MAX_STAGE: u8 = 2;

/// llama.cpp slot of the main session when `assistance.local.prompt_cache` is on; workers and side
/// requests use the others.
const MAIN_SLOT: u32 = 0;

/// Loop recovery penalties: mild enough to keep code and tool syntax intact (identifiers repeat).
const LOOP_PRESENCE_PENALTY: f64 = 0.5;
const LOOP_REPETITION_PENALTY: f64 = 1.1;

/// Measured speed samples needed before the time budget trusts them.
const TIME_BUDGET_MIN_SAMPLES: u64 = 3;

/// Session escalation state; `stage` only grows within a session.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Escalation {
    pub(super) stage: u8,
    strikes: u32,
    seeded: bool,
}

/// The local route of a configuration: what `local_server` needs to ask the server.
#[derive(Debug, Clone)]
pub(super) struct LocalRoute {
    pub(super) provider: String,
    pub(super) model: String,
    pub(super) base_url: Option<String>,
    pub(super) token: Option<String>,
    pub(super) headers: Vec<crate::llm::model_config::RequestHeader>,
}

impl LocalRoute {
    /// The route of `config` when it is served locally; `None` for hosted endpoints.
    pub(super) fn of(config: &Config) -> Option<Self> {
        if config.model.is_empty() || !config.local_main_model() {
            return None;
        }
        let selection = config.selection();
        let (base_url, token) = match &config.endpoint {
            Some(endpoint) => (
                Some(endpoint.base_url.clone()),
                crate::llm::catalog::resolve_credential(
                    &config.user_dir,
                    &endpoint.base_url,
                    endpoint.api_key_env.as_deref(),
                )
                .ok()
                .flatten(),
            ),
            None => (None, None),
        };
        Some(Self {
            provider: selection.provider,
            model: selection.model,
            base_url,
            token,
            headers: config.request_headers().to_vec(),
        })
    }

    fn server(&self) -> LocalServer {
        LocalServer::detect(
            &self.provider,
            &self.model,
            self.base_url.as_deref(),
            self.token.as_deref(),
            &self.headers,
        )
    }

    fn facts(&self) -> ServerFacts {
        local_server::facts(
            &self.provider,
            &self.model,
            self.base_url.as_deref(),
            self.token.as_deref(),
            &self.headers,
        )
    }
}

/// Per-session local-profile state, beyond escalation.
#[derive(Debug, Default)]
pub(super) struct LocalSession {
    /// Route, server kind and facts, probed once per session on first use.
    route: Option<LocalRoute>,
    server: Option<LocalServer>,
    facts: ServerFacts,
    probed: bool,
    /// Ollama: the window this machine's memory affords the model, when smaller than configured.
    affordable_context: Option<u32>,
    /// The record this model had when the session started.
    recorded: Evidence,
    /// Evidence not yet written to `local-profile.json`.
    pending: Evidence,
    /// Speeds measured this session (decode excludes requests with a draft model).
    decode: Mean,
    prefill: Mean,
    /// Phase thinking for the next request.
    think_next: bool,
    /// A successful `todo` update in the current round: the next request plans again.
    plan_changed: bool,
    /// The next request retries a repetition loop with penalties; the last one did.
    penalty_next: bool,
    penalty_sent: bool,
    /// One-time notices already shown this session.
    shown: BTreeSet<&'static str>,
    /// Whether the main model was loaded when this session first asked (`None`: unknown).
    loaded_at_start: Option<bool>,
}

/// A worker's llama.cpp slot: the main session keeps `MAIN_SLOT`; with fewer than two slots there is
/// nothing to separate and the server chooses.
pub(crate) fn worker_slot(slots: Option<u32>, worker: usize) -> Option<u32> {
    let slots = slots.filter(|n| *n > 1)?;
    Some(1 + (worker % (slots as usize - 1)) as u32)
}

/// The slot for side requests on the main model (summaries, ranking, repair): the last one, so the
/// main session's cache survives them; `None` with fewer than two slots.
pub(crate) fn side_slot(slots: Option<u32>) -> Option<u32> {
    slots.filter(|n| *n > 1).map(|n| n - 1)
}

/// A worker backend on a local llama.cpp route gets its own prompt-cache slot (when the server has
/// more than one), so it does not evict the main session's cached prompt.
pub(super) fn configure_worker_backend(config: &Config, backend: &mut Backend, worker: usize) {
    if !config.local_profile_applies() || !config.assistance.local.prompt_cache {
        return;
    }
    let Some(route) = LocalRoute::of(config) else {
        return;
    };
    if route.server() != LocalServer::LlamaCpp {
        return;
    }
    backend.set_local_hints(LocalHints {
        cache_slot: worker_slot(route.facts().slots, worker),
        prompt_progress: true,
        ..Default::default()
    });
}

/// The pre-action time limit a measured speed affords: prompt processing plus `output_tokens` of
/// generation, with a 1.5x margin and a minute of slack, within 2..=60 minutes.
pub(crate) fn time_budget_secs(
    prompt_tokens: u64,
    output_tokens: u64,
    prefill_tps: Option<f64>,
    decode_tps: f64,
) -> u64 {
    let prefill = prefill_tps
        .filter(|rate| rate.is_finite() && *rate > 0.0)
        .map_or(0.0, |rate| prompt_tokens as f64 / rate);
    let decode = output_tokens as f64 / decode_tps.max(0.1);
    ((prefill + decode) * 1.5 + 60.0).clamp(120.0, 3600.0) as u64
}

impl Agent<'_> {
    /// The local profile applies: a non-hermetic run whose main model is served locally, with
    /// `assistance.local.enabled` on.
    pub(super) fn local_profile_active(&self) -> bool {
        !self.hermetic && self.config.local_profile_applies()
    }

    /// The tier of a local route; `None` on hosted routes and hermetic runs. The small tier needs the
    /// lean profile and the local profile, so an explicit `lean_local = false` keeps the full offer.
    pub(crate) fn local_tier(&self) -> Option<Tier> {
        if self.hermetic || !self.config.local_main_model() {
            return None;
        }
        if !self.lean_local() {
            return Some(Tier::Full);
        }
        let small = self.config.assistance.local.small_window;
        Some(
            if self.config.local_profile_applies() && small > 0 && self.context_window() <= small {
                Tier::Small
            } else {
                Tier::Lean
            },
        )
    }

    /// Whether the first request offers only the minimal tool set: after a context overflow, on the
    /// small tier, or after the first escalation step.
    pub(super) fn minimal_tools(&self) -> bool {
        self.lean_tools
            || self.local_tier() == Some(Tier::Small)
            || (self.local_escalation.stage >= 1 && self.local_profile_active())
    }

    /// Escalation has reached the lean prompt step on an active local profile.
    pub(super) fn local_escalated(&self) -> bool {
        self.local_escalation.stage >= 1 && self.local_profile_active()
    }

    /// The edit grammar after the second escalation step, unless a pin decides it.
    pub(super) fn escalated_edit_mode(&self) -> Option<EditMode> {
        (self.local_escalation.stage >= 2
            && self.local_profile_active()
            && self
                .config
                .pinned_edit_mode(&self.version.harness)
                .is_none())
        .then_some(EditMode::Replace)
    }

    /// Streamed-reasoning byte cap for one request on the lean tiers: `reasoning_share` percent of the
    /// planned window at about four bytes per token.
    pub(super) fn local_reasoning_cap(&self) -> Option<usize> {
        let share = self.config.assistance.local.reasoning_share as usize;
        if share == 0
            || !self.local_profile_active()
            || !matches!(self.local_tier(), Some(Tier::Small | Tier::Lean))
        {
            return None;
        }
        Some(
            (self.context_window() as usize)
                .saturating_mul(4)
                .saturating_mul(share)
                .saturating_div(100)
                .clamp(256, 1_048_576),
        )
    }

    /// Applies `assistance.local.sampling = "model"` to one request's profile and names the source it
    /// used. Any pinned temperature wins: `[harness]`, a learned harness value or a model profile (all
    /// reach `harness_temperature`), an explicit profile temperature, or operator-owned adaptive
    /// sampling. Dial's own runtime gets the model's shipped settings when its files declare them; a
    /// local server gets no `temperature`, so its own per-model defaults apply.
    pub(super) fn apply_local_sampling(
        &self,
        profile: &mut Option<ModelProfile>,
        harness_temperature: f64,
    ) -> Option<&'static str> {
        let adaptive = &self.config.adaptive;
        if !self.local_profile_active()
            || self.config.assistance.local.sampling != LocalSamplingSource::Model
            || harness_temperature != Harness::default().temperature
            || (adaptive.enabled && (adaptive.sampling || adaptive.pin_sampling))
            || profile
                .as_ref()
                .is_some_and(|p| p.temperature.is_some() || p.thinking == Some(true))
        {
            return None;
        }
        let selection = self.auth_config.selection();
        if self.auth_config.endpoint.is_none() {
            let shipped = crate::llm::model_sampling::recommended_sampling(&self.config.model)?;
            let p = profile.get_or_insert_with(|| {
                ModelProfile::minimal(&selection.provider, &selection.model)
            });
            if p.capabilities.temperature {
                p.temperature = shipped.temperature;
            }
            if p.top_p.is_none() && shipped.top_p.is_some() {
                p.capabilities.top_p = true;
                p.top_p = shipped.top_p;
            }
            if p.provider == "mlx" && (shipped.top_k.is_some() || shipped.min_p.is_some()) {
                let local = p.local_sampling.get_or_insert_with(Default::default);
                local.top_k = local.top_k.or(shipped.top_k);
                local.min_p = local.min_p.or(shipped.min_p);
            }
            return Some(shipped.source);
        }
        let p = profile
            .get_or_insert_with(|| ModelProfile::minimal(&selection.provider, &selection.model));
        p.capabilities.temperature = false;
        Some("local server defaults")
    }

    /// Restores the escalation step this model reached in earlier sessions, once per session, and
    /// keeps its record for the draft-model trial.
    pub(super) fn seed_local_escalation(&mut self) {
        if std::mem::replace(&mut self.local_escalation.seeded, true)
            || !self.local_profile_active()
        {
            return;
        }
        self.local_session.recorded = local_evidence::read(&self.config.user_dir)
            .ok()
            .and_then(|mut records| records.remove(&self.local_model_key()))
            .unwrap_or_default();
        if self.config.assistance.local.escalate_after == 0 {
            return;
        }
        let stage = self.local_session.recorded.stage.min(MAX_STAGE);
        if stage > self.local_escalation.stage {
            self.local_escalation.stage = stage;
            self.apply_local_stage();
        }
    }

    /// Counts one tool round. `failed` is a malformed tool call or a failed edit or write. After
    /// `escalate_after` failing rounds in a row the session moves one step stricter and remembers the
    /// step for this model. Returns whether the offer changed, so the caller re-reads it.
    pub(super) fn observe_local_round(&mut self, failed: bool, ui: &mut dyn Ui) -> bool {
        if self.local_profile_active() {
            self.local_session.pending.rounds.add(!failed);
            // Planning and recovery think; a step after a successful round is mechanical.
            self.local_session.think_next =
                failed || std::mem::take(&mut self.local_session.plan_changed);
        }
        let after = self.config.assistance.local.escalate_after;
        if !self.local_profile_active() || after == 0 {
            return false;
        }
        let pinned = self
            .config
            .pinned_edit_mode(&self.version.harness)
            .is_some();
        // The second step only changes an unpinned grammar that is not already `replace`.
        let stricter = match self.local_escalation.stage {
            0 => true,
            1 => !pinned && self.edit_mode() != EditMode::Replace,
            _ => false,
        };
        let state = &mut self.local_escalation;
        if !failed {
            state.strikes = 0;
            return false;
        }
        state.strikes += 1;
        if state.strikes < after || !stricter {
            return false;
        }
        state.strikes = 0;
        state.stage += 1;
        let stage = state.stage;
        self.apply_local_stage();
        let what = if stage == 1 {
            "offering the minimal tool set and the short prompt (other tools load on demand)"
        } else {
            "switching edits to the replace grammar"
        };
        ui.event(&crate::agent::ui_event::UiEvent::new(
            crate::agent::ui_event::EventKind::Model,
            format!("Local model: {after} failing tool rounds in a row; {what}"),
        ));
        self.local_session.pending.stage = self.local_session.pending.stage.max(stage);
        if let Err(error) = self.flush_local_evidence() {
            ui.diagnostic(&format!(
                "Local profile step not remembered ({error:#}); it still applies to this session."
            ));
        }
        true
    }

    /// Counts one edit or write outcome under the edit grammar in effect.
    pub(super) fn observe_local_edit(&mut self, ok: bool) {
        if !self.local_profile_active() || !self.config.assistance.local.suggestions {
            return;
        }
        let grammar = serde_json::to_value(self.edit_mode())
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default();
        self.local_session
            .pending
            .edits
            .entry(grammar)
            .or_default()
            .add(ok);
    }

    /// Writes this session's unsaved evidence to `local-profile.json`. The escalation step is always
    /// kept (it drives the next session); counters only when `assistance.local.suggestions` is on.
    pub(super) fn flush_local_evidence(&mut self) -> Result<()> {
        if !self.local_profile_active() {
            return Ok(());
        }
        let mut delta = std::mem::take(&mut self.local_session.pending);
        if !self.config.assistance.local.suggestions {
            delta = Evidence {
                stage: delta.stage,
                ..Default::default()
            };
        }
        if delta == Evidence::default() {
            return Ok(());
        }
        local_evidence::update(&self.config.user_dir, &self.local_model_key(), &delta)
    }

    /// Probes the local server once per session: route, server kind, slots, thinking switch, the
    /// window memory affords (Ollama), and whether the model was already loaded.
    fn probe_local_server(&mut self) {
        if self.local_session.probed || !self.local_profile_active() {
            return;
        }
        self.local_session.probed = true;
        let Some(route) = LocalRoute::of(&self.auth_config) else {
            return;
        };
        let server = route.server();
        self.local_session.facts = route.facts();
        self.local_session.server = Some(server);
        if let Some(base_url) = route.base_url.as_deref() {
            self.local_session.loaded_at_start = local_server::model_loaded(
                &route.provider,
                &route.model,
                base_url,
                route.token.as_deref(),
                &route.headers,
            );
            if server == LocalServer::Ollama && self.config.assistance.local.context_advice {
                let kv_bits = None;
                self.local_session.affordable_context = local_server::affordable_context(
                    &route.model,
                    base_url,
                    route.token.as_deref(),
                    &route.headers,
                    kv_bits,
                )
                .filter(|tokens| *tokens < self.auth_config.context_window);
            }
        }
        self.local_session.route = Some(route);
    }

    /// Turn start on a local route: probe the server once, think on the first request, and say once
    /// per session what a side model on the same server costs.
    pub(super) fn begin_local_turn(&mut self, ui: &mut dyn Ui) {
        if !self.local_profile_active() {
            return;
        }
        self.probe_local_server();
        self.local_session.think_next = true;
        self.local_session.penalty_next = false;
        self.local_session.plan_changed = false;
        if self.config.assistance.local.side_models == LocalSideModels::Warn {
            for role in ["small", "coach"] {
                if !self.local_side_swap(role) || !self.local_session.shown.insert("side") {
                    continue;
                }
                let model = self
                    .config
                    .role_selection(role)
                    .map(|selection| selection.model.clone())
                    .unwrap_or_default();
                let load = self.local_load_cost();
                ui.notice(&format!(
                    "The {role} role uses {model} on the same local server as the main model, so side \
                     requests (summaries, ranking, edit repair) swap models{load}. Set \
                     assistance.local.side_models = \"reuse_main\" to keep the main model loaded, or \
                     \"allow\" to silence this."
                ));
            }
        }
    }

    /// Mean measured model-load time, as a parenthetical, when known.
    fn local_load_cost(&self) -> String {
        let recorded = &self.local_session.recorded;
        let (loads, secs) = (
            recorded.loads + self.local_session.pending.loads,
            recorded.load_secs + self.local_session.pending.load_secs,
        );
        if loads == 0 {
            String::new()
        } else {
            format!(" (a load took {:.0}s on average here)", secs / loads as f64)
        }
    }

    /// Whether `role` is a different model on the main model's local server: a request to it makes a
    /// single-model server swap weights (Ollama, LM Studio, llama.cpp router) or Dial's runtime load a
    /// second model. Compares routes only; no credential or network access.
    fn local_side_swap(&self, role: &str) -> bool {
        fn server_and_model(config: &Config) -> Option<(Option<String>, String)> {
            (!config.model.is_empty() && config.local_main_model()).then(|| {
                (
                    config.endpoint.as_ref().map(|e| e.base_url.clone()),
                    config.model.clone(),
                )
            })
        }
        if self.config.role_selection(role).is_none() {
            return false;
        }
        let (Some((main_server, main_model)), Some((side_server, side_model))) = (
            server_and_model(&self.auth_config),
            server_and_model(&self.auth_config.for_role(role)),
        ) else {
            return false;
        };
        main_server == side_server && main_model != side_model
    }

    /// `assistance.local.side_models = "reuse_main"`: a side request for `role` stays on the loaded
    /// main model instead of swapping in its own.
    pub(super) fn local_side_reuses_main(&self, role: &str) -> bool {
        self.local_profile_active()
            && self.config.assistance.local.side_models == LocalSideModels::ReuseMain
            && self.local_side_swap(role)
    }

    /// Counts a side request that swapped models on the main model's local server.
    pub(super) fn observe_local_side_request(&mut self, role: &str) {
        if self.local_profile_active() && self.local_side_swap(role) {
            self.local_session.pending.side_model_swaps += 1;
        }
    }

    /// Persistent hints for every request of the main backend: keep-loaded (unless the model profile
    /// sets its own `keep_alive`), the llama.cpp prompt slot and prompt-progress reports.
    fn local_base_hints(&self, profile: Option<&ModelProfile>) -> LocalHints {
        let local = &self.config.assistance.local;
        let server = self.local_session.server;
        let explicit_keep_alive = profile.is_some_and(|profile| profile.keep_alive.is_some());
        let llama = server == Some(LocalServer::LlamaCpp);
        LocalHints {
            keep_alive_secs: (local.keep_loaded_secs > 0
                && !explicit_keep_alive
                && matches!(server, Some(LocalServer::Ollama | LocalServer::LmStudio)))
            .then_some(local.keep_loaded_secs),
            cache_slot: (llama && local.prompt_cache).then_some(MAIN_SLOT),
            prompt_progress: llama,
            ..Default::default()
        }
    }

    /// Whether this request's profile already decides thinking; then phase thinking stays out.
    fn thinking_pinned(profile: Option<&ModelProfile>) -> bool {
        profile.is_some_and(|profile| {
            profile.thinking.is_some()
                || profile.reasoning_effort.is_some()
                || profile.thinking_budget.is_some()
                || profile
                    .local_sampling
                    .as_ref()
                    .and_then(|sampling| sampling.chat_template_kwargs.as_ref())
                    .is_some_and(|kwargs| kwargs.contains_key("enable_thinking"))
        })
    }

    /// Prepares one main request on a local route: base and per-request hints on the backend, and an
    /// Ollama window bounded by the memory estimate when no window is pinned. Returns a short label of
    /// what this request carries, for the request record.
    pub(super) fn prepare_local_request(
        &mut self,
        backend: &mut Backend,
        profile: &mut Option<ModelProfile>,
    ) -> Option<Value> {
        if !self.local_profile_active() {
            return None;
        }
        self.probe_local_server();
        backend.set_local_hints(self.local_base_hints(profile.as_ref()));
        let local = &self.config.assistance.local;
        backend.set_loop_penalty(local.loop_penalty.then(|| {
            LocalHints {
                presence_penalty: Some(LOOP_PRESENCE_PENALTY),
                repetition_penalty: Some(LOOP_REPETITION_PENALTY),
                thinking: (self.local_session.facts.thinking_switch == Some(true)
                    && !Self::thinking_pinned(profile.as_ref()))
                .then_some(false),
                ..Default::default()
            }
        }));
        let mut hints = LocalHints::default();
        if local.thinking == LocalThinking::Phase
            && self.local_session.facts.thinking_switch == Some(true)
            && !Self::thinking_pinned(profile.as_ref())
        {
            hints.thinking = Some(self.local_session.think_next);
        }
        let penalty = std::mem::take(&mut self.local_session.penalty_next) && local.loop_penalty;
        if penalty {
            hints.presence_penalty = Some(LOOP_PRESENCE_PENALTY);
            hints.repetition_penalty = Some(LOOP_REPETITION_PENALTY);
        }
        self.local_session.penalty_sent = penalty;
        if let Some(tokens) = self.local_session.affordable_context
            && profile.as_ref().is_none_or(|p| p.context_window.is_none())
        {
            let selection = self.auth_config.selection();
            profile
                .get_or_insert_with(|| ModelProfile::minimal(&selection.provider, &selection.model))
                .context_window = Some(tokens);
        }
        let record = json!({
            "thinking": hints.thinking,
            "loop_penalty": penalty,
            "num_ctx": self.local_session.affordable_context,
        });
        backend.next_local_hints(hints);
        Some(record)
    }

    /// A side request on the main backend: its own llama.cpp slot when the server has one to spare,
    /// and no thinking where the model has a switch and nothing pins it.
    pub(super) fn prepare_local_side_request(&mut self, backend: &mut Backend) {
        if !self.local_profile_active() {
            return;
        }
        self.probe_local_server();
        let profile = backend.model_profile().cloned();
        backend.set_local_hints(self.local_base_hints(profile.as_ref()));
        let llama = self.local_session.server == Some(LocalServer::LlamaCpp);
        backend.next_local_hints(LocalHints {
            cache_slot: (llama && self.config.assistance.local.prompt_cache)
                .then(|| side_slot(self.local_session.facts.slots))
                .flatten(),
            thinking: (self.config.assistance.local.thinking == LocalThinking::Phase
                && self.local_session.facts.thinking_switch == Some(true)
                && !Self::thinking_pinned(profile.as_ref()))
            .then_some(false),
            ..Default::default()
        });
    }

    /// Records one main reply: speeds (with a draft model in effect, as its trial), prompt-cache
    /// reuse, model loads and the outcome of a loop penalty retry.
    pub(super) fn observe_local_reply(&mut self, reply: &llm::Reply, backend: &mut Backend) {
        if !self.local_profile_active() {
            return;
        }
        let retries = backend.take_loop_penalty_retries();
        let session = &mut self.local_session;
        let evidence = &mut session.pending;
        evidence.requests += 1;
        if retries > 0 {
            evidence.repetition_loops += u64::from(retries);
            evidence.penalty_retries.add(true);
        }
        if std::mem::take(&mut session.penalty_sent) {
            evidence.penalty_retries.add(true);
        }
        let draft = backend
            .model_profile()
            .and_then(|profile| profile.local_runtime.as_ref())
            .and_then(|runtime| runtime.draft_model.clone());
        if let Some(rate) = reply.decode_tokens_per_second {
            match draft {
                Some(draft_model) => {
                    let trial = evidence.draft.get_or_insert_with(Default::default);
                    if trial.draft_model != draft_model {
                        *trial = local_evidence::DraftTrial {
                            draft_model: draft_model.clone(),
                            ..Default::default()
                        };
                        // The baseline comes from the record once, when this draft is first tried.
                        let recorded = session.recorded.draft.as_ref();
                        if recorded.is_none_or(|known| known.draft_model != draft_model) {
                            trial.baseline = session.recorded.decode_tps;
                        }
                    }
                    trial.with_draft.add(rate);
                }
                None => {
                    evidence.decode_tps.add(rate);
                    session.decode.add(rate);
                }
            }
        }
        if let Some(rate) = reply.prefill_tokens_per_second {
            evidence.prefill_tps.add(rate);
            session.prefill.add(rate);
        }
        if let (Some(prompt), Some(cached)) = (reply.prompt_tokens, reply.cached_prompt_tokens) {
            evidence.reported_prompt_tokens += prompt;
            evidence.cached_prompt_tokens += cached.min(prompt);
        }
        // Short load times are a warm server answering; a real load takes seconds.
        if let Some(secs) = reply.load_secs.filter(|secs| *secs >= 1.0) {
            evidence.loads += 1;
            evidence.load_secs += secs;
        }
    }

    /// A failed main request: a repetition loop arms the penalty retry and thinking off for the
    /// retry; any other intercepted generation turns thinking off for the retry; an overflow counts.
    pub(super) fn observe_local_failure(&mut self, error: &anyhow::Error, backend: &mut Backend) {
        if !self.local_profile_active() {
            return;
        }
        let retries = backend.take_loop_penalty_retries();
        let session = &mut self.local_session;
        session.pending.repetition_loops += u64::from(retries);
        if retries > 0 || std::mem::take(&mut session.penalty_sent) {
            session.pending.penalty_retries.add(false);
        }
        if let Some(hit) = error.downcast_ref::<super::reasoning::GateHit>() {
            session.think_next = false;
            if hit.is_repetition() {
                session.pending.repetition_loops += 1;
                session.penalty_next = true;
            } else {
                session.pending.reasoning_capped += 1;
            }
        } else if looks_like_overflow(error) {
            session.pending.overflows += 1;
        }
    }

    /// After a plan change (a successful `todo` update) the next request plans again.
    pub(super) fn observe_local_plan_change(&mut self) {
        if !self.local_profile_active() {
            return;
        }
        self.local_session.plan_changed = true;
    }

    /// Ollama: the window this machine's memory affords the model, when it is below the configured
    /// window and `assistance.local.context_advice` is on.
    pub(super) fn local_affordable_context(&self) -> Option<u32> {
        self.local_session
            .affordable_context
            .filter(|_| self.local_profile_active())
    }

    /// `assistance.local.time_budget`: the pre-action time limit for one request from this model's
    /// measured speed, when the configured limit is 0 (unlimited) and enough samples exist.
    pub(super) fn local_time_budget(&self, prompt_tokens: u64, output_tokens: u64) -> Option<u64> {
        if !self.local_profile_active() || !self.config.assistance.local.time_budget {
            return None;
        }
        let session = &self.local_session;
        let decode = if session.decode.samples >= TIME_BUDGET_MIN_SAMPLES {
            session.decode
        } else {
            session.recorded.decode_tps
        };
        if decode.samples < TIME_BUDGET_MIN_SAMPLES || decode.mean <= 0.0 {
            return None;
        }
        let prefill = if session.prefill.samples > 0 {
            session.prefill
        } else {
            session.recorded.prefill_tps
        };
        Some(time_budget_secs(
            prompt_tokens,
            output_tokens,
            (prefill.samples > 0).then_some(prefill.mean),
            decode.mean,
        ))
    }

    /// `assistance.local.context_advice`: one notice per session when the server serves a smaller
    /// window than configured, saying how to raise it for this server; and the Ollama memory bound.
    pub(super) fn local_context_advice(&mut self, served: u32, ui: &mut dyn Ui) {
        if !self.local_profile_active() {
            return;
        }
        self.probe_local_server();
        self.local_session.pending.served_context = Some(served);
        let configured = self.auth_config.context_window;
        if !self.config.assistance.local.context_advice {
            return;
        }
        if let Some(affordable) = self.local_session.affordable_context
            && self.local_session.shown.insert("ollama-memory")
        {
            ui.notice(&format!(
                "Ollama: this machine's memory fits about {affordable} tokens of context for {}; Dial \
                 requests num_ctx = {affordable} instead of {configured} so the model is not \
                 offloaded or swapped. Set context_window in /model settings to choose another size.",
                self.auth_config.model
            ));
        }
        if served >= configured
            || self.local_session.affordable_context == Some(served)
            || !self.local_session.shown.insert("served-context")
        {
            return;
        }
        let how = match self.local_session.server {
            Some(LocalServer::LmStudio) => {
                "raise Context Length in LM Studio's load settings for this model and reload it"
            }
            Some(LocalServer::LlamaCpp) => {
                "start llama-server with a larger -c (it is divided across --parallel slots)"
            }
            Some(LocalServer::Ollama) => {
                "Dial asks Ollama for the configured window on every request; if it stays small, \
                 set OLLAMA_CONTEXT_LENGTH or check the model's memory fit"
            }
            _ => "raise the context length in the server's model settings",
        };
        ui.notice(&format!(
            "The local server serves {served} tokens of context; Dial is configured for {configured} \
             and now plans against {served}. To use more, {how}; or set context_window = {served} in \
             /model settings to make this the plan."
        ));
    }

    /// Session end on a local route: save evidence, and unload a model this session loaded on a
    /// server whose keep-alive Dial extended (Ollama), so it does not hold memory after Dial exits.
    /// Runs at most once per session.
    pub fn end_local_session(&mut self) {
        let _ = self.flush_local_evidence();
        let loaded_at_start = self.local_session.loaded_at_start.take();
        let local = &self.config.assistance.local;
        if !self.local_profile_active()
            || local.keep_loaded_secs == 0
            || loaded_at_start != Some(false)
            || self.local_session.server != Some(LocalServer::Ollama)
            || self
                .local_base_hints(self.backend.as_ref().and_then(Backend::model_profile))
                .keep_alive_secs
                .is_none()
        {
            return;
        }
        if let Some(route) = &self.local_session.route
            && let Some(base_url) = route.base_url.as_deref()
        {
            let _ = local_server::unload(
                &route.provider,
                &route.model,
                base_url,
                route.token.as_deref(),
                &route.headers,
            );
        }
    }

    /// Applies the current step: the grammar switch reaches the workspace, then the prompt and offer.
    fn apply_local_stage(&mut self) {
        if let Some(mode) = self.escalated_edit_mode()
            && self.workspace.edit_mode() != mode
        {
            self.workspace = self.workspace.clone().with_edit_mode(mode);
        }
        self.rebuild_system();
    }

    fn local_model_key(&self) -> String {
        let selection = self.auth_config.selection();
        local_evidence::model_key(&selection.provider, &selection.model)
    }
}

#[cfg(test)]
mod tests;
