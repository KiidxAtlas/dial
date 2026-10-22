//! Runtime accounting of reported usage, shared across parent and worker backends.
use super::Reply;
use anyhow::{Result, ensure};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

/// Why inference was requested, independent of model routing and main/worker ownership.
/// Bounded keys keep reports comparable and never contain prompts or operator role names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    #[default]
    Agent,
    Retrieval,
    Compaction,
    Memory,
    Repair,
    Judgment,
    Auxiliary,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CostCoverage {
    #[default]
    NoRequests,
    Unpriced,
    NonInvoice,
    Partial,
    ObservedPriced,
}
/// Counters cover this agent runtime only, not an invoice or restored session history.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Counts {
    /// Logical backend calls; validation/auth failures may send no transport request.
    pub requests: u64,
    pub transport_attempts: u64,
    pub retries: u64,
    pub successful_requests: u64,
    pub reported_responses: u64,
    /// Failed/discarded attempts with an exact usage snapshot, not necessarily final usage.
    pub partial_reported_attempts: u64,
    pub failed_requests: u64,
    pub discarded_attempts: u64,
    pub unreported_attempts: u64,
    pub priced_requests: u64,
    pub unpriced_requests: u64,
    pub local_requests: u64,
    pub subscription_requests: u64,
    pub observed_cost_usd: Option<f64>,
    pub cost_coverage: CostCoverage,
    pub cache_reported_responses: u64,
    pub reasoning_reported_responses: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
}
impl Counts {
    fn add(self, delta: Self) -> Result<Self> {
        let mut out = self;
        macro_rules! add {
            ($($field:ident),+ $(,)?) => {$(
                out.$field = self.$field.checked_add(delta.$field)
                    .ok_or_else(|| anyhow::anyhow!("usage accounting counter overflow: {}", stringify!($field)))?;
            )+};
        }
        add!(
            requests,
            transport_attempts,
            retries,
            successful_requests,
            reported_responses,
            partial_reported_attempts,
            failed_requests,
            discarded_attempts,
            unreported_attempts,
            priced_requests,
            unpriced_requests,
            local_requests,
            subscription_requests,
            cache_reported_responses,
            reasoning_reported_responses,
            input,
            output,
            cache_read,
            cache_write,
            reasoning
        );
        if let Some(cost) = delta.observed_cost_usd {
            let sum = self.observed_cost_usd.unwrap_or(0.0) + cost;
            ensure!(
                cost.is_finite() && cost >= 0.0 && sum.is_finite(),
                "usage accounting cost overflow or invalid observed cost"
            );
            out.observed_cost_usd = Some(sum);
        }
        Ok(out)
    }
    fn refresh_coverage(&mut self) {
        self.cost_coverage = if self.requests == 0 {
            CostCoverage::NoRequests
        } else if self.observed_cost_usd.is_none() {
            if self.local_requests == self.requests
                || self.subscription_requests == self.requests
                || self.requests.checked_sub(self.local_requests)
                    == Some(self.subscription_requests)
            {
                CostCoverage::NonInvoice
            } else {
                CostCoverage::Unpriced
            }
        } else if self.unpriced_requests > 0
            || self.local_requests > 0
            || self.subscription_requests > 0
            || self.unreported_attempts > 0
            || self.requests.checked_sub(self.successful_requests) != Some(self.failed_requests)
        {
            CostCoverage::Partial
        } else {
            CostCoverage::ObservedPriced
        };
    }
    pub fn cost_label(&self) -> String {
        match (self.observed_cost_usd, self.cost_coverage) {
            (Some(cost), CostCoverage::ObservedPriced) => {
                format!("${cost:.6} USD observed API cost")
            }
            (Some(cost), _) => format!("${cost:.6} USD observed API cost (partial)"),
            (None, CostCoverage::NoRequests) => "no observed requests".into(),
            (None, CostCoverage::NonInvoice) => {
                "local/subscription requests are not invoice-priced".into()
            }
            (None, _) => "API cost unknown (pricing or accepted usage unavailable)".into(),
        }
    }
}
#[derive(Clone, Copy)]
pub(super) enum Billing {
    Api,
    Local,
    Subscription,
}
#[derive(Clone, Debug, Serialize)]
pub struct Coverage {
    pub history: &'static str,
    pub attribution: &'static str,
    pub cost: &'static str,
    pub attempts: &'static str,
    pub turn_id: Option<u64>,
}
impl Default for Coverage {
    fn default() -> Self {
        Self {
            history: "current_agent_runtime_only; prior/resumed accounting unavailable",
            attribution: "original_runtime_turn; late worker results retain their owner",
            cost: "observed accepted-response prices, not invoice totals; caps are observed-cost stop boundaries",
            attempts: "model and selected-memory service send invocations; failed stream snapshots retain exact reported fields but remain incomplete/unpriced; missing attempt usage unavailable; excludes auth, discovery, server startup and redirects",
            turn_id: None,
        }
    }
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct Groups {
    /// Incrementally maintained aggregate; no history scan is needed for presentation.
    pub all: Counts,
    pub main: Counts,
    pub workers: Counts,
    pub by_model: BTreeMap<String, ModelCounts>,
    /// Orthogonal breakdown: these counters sum to `all`, not additional usage.
    pub by_purpose: BTreeMap<Purpose, Counts>,
    pub coverage: Coverage,
}
impl Groups {
    fn selected(&self, worker: bool) -> Counts {
        if worker { self.workers } else { self.main }
    }
    fn validate(&self, scope: &Scope, delta: Counts, reported: bool) -> Result<()> {
        let model = scope.model.as_deref().map(|model| {
            self.by_model
                .get(model)
                .map_or(Counts::default(), |counts| {
                    if scope.worker {
                        counts.workers
                    } else {
                        counts.main
                    }
                })
        });
        for counts in [
            self.all,
            self.selected(scope.worker),
            self.by_purpose
                .get(&scope.purpose)
                .copied()
                .unwrap_or_default(),
        ]
        .into_iter()
        .chain(model)
        {
            counts.add(delta)?;
            ensure!(
                !reported || counts.unreported_attempts > 0,
                "reported usage has no matching transport attempt in its scope"
            );
        }
        Ok(())
    }
    fn apply(&mut self, scope: &Scope, delta: Counts, reported: bool) {
        let update = |counts: &mut Counts| {
            *counts = counts
                .add(delta)
                .expect("accounting update was validated under lock");
            if reported {
                counts.unreported_attempts -= 1;
            }
            counts.refresh_coverage();
        };
        update(&mut self.all);
        update(if scope.worker {
            &mut self.workers
        } else {
            &mut self.main
        });
        update(self.by_purpose.entry(scope.purpose).or_default());
        if let Some(model) = scope.model.as_deref() {
            let counts = self.by_model.entry(model.to_owned()).or_default();
            update(if scope.worker {
                &mut counts.workers
            } else {
                &mut counts.main
            });
        }
    }
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct ModelCounts {
    pub main: Counts,
    pub workers: Counts,
}
#[derive(Default)]
struct State {
    next: u64,
    total: Groups,
    turns: BTreeMap<u64, Groups>,
    reserved_workers: BTreeMap<u64, u64>,
}
#[derive(Clone, Default)]
pub struct Scope {
    state: Arc<Mutex<State>>,
    turn: u64,
    worker: bool,
    worker_limit: u64,
    model: Option<Arc<str>>,
    purpose: Purpose,
}
impl Scope {
    pub fn next_turn(&self, worker_limit: u32) -> Result<Self> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let turn = state
            .next
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("usage accounting turn counter overflow"))?;
        state.next = turn;
        let mut groups = Groups::default();
        groups.coverage.turn_id = Some(turn);
        state.turns.insert(turn, groups);
        Ok(Self {
            state: self.state.clone(),
            turn,
            worker: false,
            worker_limit: u64::from(worker_limit),
            model: None,
            purpose: Purpose::Agent,
        })
    }
    pub fn for_model(&self, model: String) -> Self {
        Self {
            model: Some(Arc::from(model)),
            ..self.clone()
        }
    }
    pub fn for_purpose(&self, purpose: Purpose) -> Self {
        Self {
            purpose,
            ..self.clone()
        }
    }
    pub fn worker(&self) -> Self {
        Self {
            worker: true,
            ..self.clone()
        }
    }
    /// Atomically reserve a worker request slot before an external memory-service call.
    pub fn reserve_external(&self, model: String) -> Result<ExternalUsageReceipt> {
        let scope = self.for_model(model);
        let mut state = scope.state.lock().unwrap_or_else(|e| e.into_inner());
        let committed_workers = state.turns.entry(scope.turn).or_default().workers.requests;
        if scope.worker && scope.worker_limit != 0 {
            let reserved = state
                .reserved_workers
                .get(&scope.turn)
                .copied()
                .unwrap_or(0);
            ensure!(
                committed_workers
                    .checked_add(reserved)
                    .is_some_and(|n| n < scope.worker_limit),
                "shared worker request budget exhausted for this turn"
            );
        }
        let reserved_worker = scope.worker;
        if reserved_worker {
            let reserved = state.reserved_workers.entry(scope.turn).or_default();
            *reserved = reserved
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("external request reservation overflow"))?;
        }
        drop(state);
        Ok(ExternalUsageReceipt {
            scope,
            active: true,
            reserved_worker,
            started: false,
        })
    }
    /// A received response rejected by its owner remains successful physical usage.
    pub fn record_discarded_response(&self) -> Result<()> {
        self.observe(
            Counts {
                discarded_attempts: 1,
                ..Counts::default()
            },
            false,
        )
    }
    fn observe(&self, delta: Counts, reported: bool) -> Result<()> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let committed_workers = s.turns.entry(self.turn).or_default().workers.requests;
        let reserved_workers = s.reserved_workers.get(&self.turn).copied().unwrap_or(0);
        ensure!(
            delta.requests == 0
                || !self.worker
                || self.worker_limit == 0
                || committed_workers
                    .checked_add(reserved_workers)
                    .is_some_and(|n| n < self.worker_limit),
            "shared worker request budget exhausted for this turn"
        );
        s.turns[&self.turn].validate(self, delta, reported)?;
        s.total.validate(self, delta, reported)?;
        s.turns
            .get_mut(&self.turn)
            .unwrap()
            .apply(self, delta, reported);
        s.total.apply(self, delta, reported);
        Ok(())
    }
    pub(super) fn begin(&self, billing: Billing) -> Result<()> {
        self.observe(
            Counts {
                requests: 1,
                local_requests: u64::from(matches!(billing, Billing::Local)),
                subscription_requests: u64::from(matches!(billing, Billing::Subscription)),
                ..Counts::default()
            },
            false,
        )
    }
    fn attempt(&self, retry: bool) -> Result<()> {
        self.observe(
            Counts {
                transport_attempts: 1,
                unreported_attempts: 1,
                retries: u64::from(retry),
                ..Counts::default()
            },
            false,
        )
    }
    pub(super) fn finish(
        &self,
        reply: Option<&Reply>,
        cost: Option<f64>,
        billing: Billing,
        attempts: u64,
    ) -> Result<()> {
        let mut delta = Counts {
            successful_requests: u64::from(reply.is_some()),
            failed_requests: u64::from(reply.is_none()),
            discarded_attempts: attempts.saturating_sub(u64::from(reply.is_some())),
            priced_requests: u64::from(cost.is_some()),
            unpriced_requests: u64::from(matches!(billing, Billing::Api) && cost.is_none()),
            observed_cost_usd: cost,
            ..Counts::default()
        };
        let mut reported = false;
        if let Some(r) = reply {
            reported = attempts > 0 && r.prompt_tokens.is_some() && r.completion_tokens.is_some();
            delta.reported_responses = u64::from(reported);
            delta.cache_reported_responses =
                u64::from(r.cached_prompt_tokens.is_some() || r.cache_creation_tokens.is_some());
            delta.reasoning_reported_responses = u64::from(r.reasoning_tokens.is_some());
            delta.input = r.prompt_tokens.unwrap_or(0);
            delta.output = r.completion_tokens.unwrap_or(0);
            delta.cache_read = r.cached_prompt_tokens.unwrap_or(0);
            delta.cache_write = r.cache_creation_tokens.unwrap_or(0);
            delta.reasoning = r.reasoning_tokens.unwrap_or(0);
        }
        self.observe(delta, reported)
    }
    pub fn snapshot(&self) -> (Groups, Groups) {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (
            s.total.clone(),
            s.turns.get(&self.turn).cloned().unwrap_or_default(),
        )
    }
    /// Cumulative accepted priced usage, including late/discarded maintenance workers.
    pub fn observed_cost(&self) -> Option<f64> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .total
            .all
            .observed_cost_usd
    }
    pub fn observed_turn_cost(&self) -> Option<f64> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .turns
            .get(&self.turn)
            .and_then(|turn| turn.all.observed_cost_usd)
    }
}
pub struct ExternalUsageReceipt {
    scope: Scope,
    active: bool,
    reserved_worker: bool,
    started: bool,
}
impl ExternalUsageReceipt {
    /// Commit the reserved request immediately before sending. A lost response still consumes
    /// its request slot and leaves an honest unreported transport attempt.
    pub fn mark_started(&mut self) -> Result<()> {
        ensure!(self.active, "external usage receipt is already settled");
        if self.started {
            return Ok(());
        }
        let delta = Counts {
            requests: 1,
            transport_attempts: 1,
            unreported_attempts: 1,
            ..Counts::default()
        };
        let mut state = self.scope.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .turns
            .entry(self.scope.turn)
            .or_default()
            .validate(&self.scope, delta, false)?;
        state.total.validate(&self.scope, delta, false)?;
        if self.reserved_worker {
            release_external_reservation(&mut state, self.scope.turn)?;
            self.reserved_worker = false;
        }
        state
            .turns
            .get_mut(&self.scope.turn)
            .unwrap()
            .apply(&self.scope, delta, false);
        state.total.apply(&self.scope, delta, false);
        self.started = true;
        Ok(())
    }
    /// Commit exact service usage and the independent owner result-disposition once.
    pub fn account(
        mut self,
        usage: &serde_json::Value,
        accepted: bool,
        discarded: bool,
    ) -> Result<()> {
        ensure!(
            self.started,
            "external receipt was not started before its response"
        );
        let number = |key: &str| usage.get(key).and_then(serde_json::Value::as_u64);
        let prompt = number("prompt_tokens");
        let output = number("completion_tokens");
        let reported = prompt.is_some() && output.is_some();
        let cost = usage
            .get("cost")
            .and_then(serde_json::Value::as_f64)
            .filter(|cost| cost.is_finite() && *cost >= 0.0);
        let delta = Counts {
            successful_requests: u64::from(accepted),
            failed_requests: u64::from(!accepted),
            discarded_attempts: u64::from(!accepted || discarded),
            priced_requests: u64::from(cost.is_some()),
            unpriced_requests: u64::from(cost.is_none()),
            observed_cost_usd: cost,
            reported_responses: u64::from(reported),
            cache_reported_responses: u64::from(
                number("cached_prompt_tokens").is_some()
                    || number("cache_creation_tokens").is_some(),
            ),
            reasoning_reported_responses: u64::from(number("reasoning_tokens").is_some()),
            input: prompt.unwrap_or(0),
            output: output.unwrap_or(0),
            cache_read: number("cached_prompt_tokens").unwrap_or(0),
            cache_write: number("cache_creation_tokens").unwrap_or(0),
            reasoning: number("reasoning_tokens").unwrap_or(0),
            ..Counts::default()
        };
        let mut state = self.scope.state.lock().unwrap_or_else(|e| e.into_inner());
        let turn = state.turns.entry(self.scope.turn).or_default();
        turn.validate(&self.scope, delta, reported)?;
        state.total.validate(&self.scope, delta, reported)?;
        if self.reserved_worker {
            release_external_reservation(&mut state, self.scope.turn)?;
        }
        state
            .turns
            .get_mut(&self.scope.turn)
            .unwrap()
            .apply(&self.scope, delta, reported);
        state.total.apply(&self.scope, delta, reported);
        self.active = false;
        Ok(())
    }
}
impl Drop for ExternalUsageReceipt {
    fn drop(&mut self) {
        if self.active {
            let mut state = self.scope.state.lock().unwrap_or_else(|e| e.into_inner());
            if self.reserved_worker {
                let _ = release_external_reservation(&mut state, self.scope.turn);
            }
            if self.started {
                let delta = Counts {
                    failed_requests: 1,
                    discarded_attempts: 1,
                    unpriced_requests: 1,
                    ..Counts::default()
                };
                if state
                    .turns
                    .get(&self.scope.turn)
                    .is_some_and(|turn| turn.validate(&self.scope, delta, false).is_ok())
                    && state.total.validate(&self.scope, delta, false).is_ok()
                {
                    state
                        .turns
                        .get_mut(&self.scope.turn)
                        .unwrap()
                        .apply(&self.scope, delta, false);
                    state.total.apply(&self.scope, delta, false);
                }
            }
            self.active = false;
        }
    }
}
fn release_external_reservation(state: &mut State, turn: u64) -> Result<()> {
    let count = state
        .reserved_workers
        .get_mut(&turn)
        .ok_or_else(|| anyhow::anyhow!("external request reservation was lost"))?;
    ensure!(*count > 0, "external request reservation underflow");
    *count -= 1;
    if *count == 0 {
        state.reserved_workers.remove(&turn);
    }
    Ok(())
}

// Provider exchanges are synchronous on the calling thread. This guard scopes only model
// transport sends, including all retries, without threading accounting through every adapter.
thread_local! {
    static TRANSPORT: std::cell::RefCell<Option<AttemptState>> = const { std::cell::RefCell::new(None) };
}
pub(super) struct Transport {
    previous: Option<AttemptState>,
}
impl Transport {
    pub(super) fn enter(scope: Option<Scope>) -> Self {
        Self {
            previous: TRANSPORT.with(|slot| {
                slot.replace(scope.map(|scope| AttemptState {
                    scope,
                    attempts: 0,
                    usage: None,
                }))
            }),
        }
    }
    pub(super) fn suspend() -> Self {
        Self {
            previous: TRANSPORT.with(|slot| slot.replace(None)),
        }
    }
    pub(super) fn attempts(&self) -> u64 {
        TRANSPORT.with(|slot| slot.borrow().as_ref().map_or(0, |state| state.attempts))
    }
    /// The accepted final Reply owns its usage; only rejected attempts retain snapshots.
    pub(super) fn settle(&self, accepted: bool) -> Result<()> {
        TRANSPORT.with(|slot| {
            if let Some(state) = slot.borrow_mut().as_mut() {
                if accepted {
                    state.usage = None;
                } else {
                    state.retain_usage()?;
                }
            }
            Ok(())
        })
    }
}
impl Drop for Transport {
    fn drop(&mut self) {
        TRANSPORT.with(|slot| slot.replace(self.previous.take()));
    }
}
pub(super) fn transport_attempt() -> Result<()> {
    TRANSPORT.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(state) = slot.as_mut() {
            let next = state
                .attempts
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("usage accounting attempt counter overflow"))?;
            state.retain_usage()?;
            state.scope.attempt(state.attempts > 0)?;
            state.attempts = next;
        }
        Ok(())
    })
}

/// Only five optional counters are retained, never response bodies or a retry history.
#[derive(Clone, Copy, Default)]
pub(super) struct Snapshot {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub reasoning: Option<u64>,
}
impl Snapshot {
    pub(super) fn from_reply(reply: &Reply) -> Self {
        Self {
            input: reply.prompt_tokens,
            output: reply.completion_tokens,
            cache_read: reply.cached_prompt_tokens,
            cache_write: reply.cache_creation_tokens,
            reasoning: reply.reasoning_tokens,
        }
    }
}
struct AttemptState {
    scope: Scope,
    attempts: u64,
    usage: Option<Snapshot>,
}
impl AttemptState {
    fn retain_usage(&mut self) -> Result<()> {
        if let Some(usage) = self.usage {
            self.scope.observe(
                Counts {
                    partial_reported_attempts: 1,
                    input: usage.input.unwrap_or(0),
                    output: usage.output.unwrap_or(0),
                    cache_read: usage.cache_read.unwrap_or(0),
                    cache_write: usage.cache_write.unwrap_or(0),
                    reasoning: usage.reasoning.unwrap_or(0),
                    cache_reported_responses: u64::from(
                        usage.cache_read.is_some() || usage.cache_write.is_some(),
                    ),
                    reasoning_reported_responses: u64::from(usage.reasoning.is_some()),
                    ..Counts::default()
                },
                false,
            )?;
            // A start/delta snapshot is not proof of complete usage, even with both fields.
            // Keep the attempt unreported and unpriced rather than inventing final totals.
            self.usage = None;
        }
        Ok(())
    }
}
pub(super) fn capture(usage: Snapshot) {
    if [
        usage.input,
        usage.output,
        usage.cache_read,
        usage.cache_write,
        usage.reasoning,
    ]
    .iter()
    .all(Option::is_none)
    {
        return;
    }
    TRANSPORT.with(|slot| {
        if let Some(state) = slot
            .borrow_mut()
            .as_mut()
            .filter(|state| state.attempts > 0)
        {
            state.usage = Some(match state.usage {
                Some(previous) => Snapshot {
                    input: usage.input.or(previous.input),
                    output: usage.output.or(previous.output),
                    cache_read: usage.cache_read.or(previous.cache_read),
                    cache_write: usage.cache_write.or(previous.cache_write),
                    reasoning: usage.reasoning.or(previous.reasoning),
                },
                None => usage,
            });
        }
    });
}
pub(super) fn capture_reply(reply: &Reply) {
    capture(Snapshot::from_reply(reply));
}
#[cfg(test)]
mod tests;
