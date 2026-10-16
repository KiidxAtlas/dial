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
            attempts: "model and selected-memory service send invocations; known dropped-response usage retained, missing attempt usage unavailable; excludes auth, discovery, server startup and redirects",
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
    static TRANSPORT: std::cell::RefCell<Option<(Scope, u64)>> = const { std::cell::RefCell::new(None) };
}
pub(super) struct Transport {
    previous: Option<(Scope, u64)>,
}
impl Transport {
    pub(super) fn enter(scope: Option<Scope>) -> Self {
        Self {
            previous: TRANSPORT.with(|slot| slot.replace(scope.map(|scope| (scope, 0)))),
        }
    }
    pub(super) fn suspend() -> Self {
        Self {
            previous: TRANSPORT.with(|slot| slot.replace(None)),
        }
    }
    pub(super) fn attempts(&self) -> u64 {
        TRANSPORT.with(|slot| slot.borrow().as_ref().map_or(0, |(_, attempts)| *attempts))
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
        if let Some((scope, attempts)) = slot.as_mut() {
            let next = attempts
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("usage accounting attempt counter overflow"))?;
            scope.attempt(*attempts > 0)?;
            *attempts = next;
        }
        Ok(())
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn worker_budget_is_shared_and_late_usage_stays_in_its_original_turn() {
        let scope = Scope::default().next_turn(2).unwrap();
        let worker = scope.worker();
        worker.begin(Billing::Api).unwrap();
        worker.attempt(false).unwrap();
        worker.attempt(true).unwrap();
        let second = worker.clone();
        second.begin(Billing::Api).unwrap();
        second.attempt(false).unwrap();
        assert!(second.begin(Billing::Api).is_err());
        let next = scope.next_turn(2).unwrap();
        worker
            .finish(
                Some(&Reply {
                    prompt_tokens: Some(12),
                    completion_tokens: Some(3),
                    ..Reply::default()
                }),
                None,
                Billing::Api,
                2,
            )
            .unwrap();
        second.finish(None, None, Billing::Api, 1).unwrap();
        let (total, current) = next.snapshot();
        assert_eq!(total.workers.requests, 2);
        assert_eq!(total.workers.input, 12);
        assert_eq!(total.workers.retries, 1);
        assert_eq!(total.workers.reported_responses, 1);
        assert_eq!(total.workers.failed_requests, 1);
        assert_eq!(current.workers.requests, 0);
        assert_eq!(scope.snapshot().1.workers.input, 12);
    }

    #[test]
    fn models_keep_separate_partial_usage_and_late_workers_keep_their_task() {
        let task = Scope::default().next_turn(2).unwrap();
        let main = task.for_model("openai-compatible/a".into());
        let worker = task.worker().for_model("anthropic/b".into());
        main.begin(Billing::Api).unwrap();
        main.attempt(false).unwrap();
        main.finish(
            Some(&Reply {
                prompt_tokens: Some(10),
                completion_tokens: Some(2),
                ..Reply::default()
            }),
            None,
            Billing::Api,
            1,
        )
        .unwrap();
        worker.begin(Billing::Api).unwrap();
        worker.attempt(false).unwrap();
        worker.attempt(true).unwrap();
        let next = task.next_turn(2).unwrap();
        worker
            .finish(
                Some(&Reply {
                    completion_tokens: Some(3),
                    cached_prompt_tokens: Some(40),
                    ..Reply::default()
                }),
                None,
                Billing::Api,
                2,
            )
            .unwrap();
        let (total, current) = task.snapshot();
        assert_eq!(current.by_model["openai-compatible/a"].main.input, 10);
        assert_eq!(current.by_model["anthropic/b"].workers.output, 3);
        assert_eq!(
            current.by_model["anthropic/b"].workers.reported_responses,
            0
        );
        assert_eq!(
            current.by_model["anthropic/b"]
                .workers
                .cache_reported_responses,
            1
        );
        assert_eq!(current.by_model["anthropic/b"].workers.retries, 1);
        assert_eq!(total.workers.cache_read, 40);
        assert!(next.snapshot().1.by_model.is_empty());
    }

    #[test]
    fn purposes_partition_usage_without_changing_worker_budgets_or_turn_ownership() {
        let root = Scope::default().next_turn(2).unwrap();
        let rank = root
            .worker()
            .for_purpose(Purpose::Retrieval)
            .for_model("shared".into());
        let compact = root
            .worker()
            .for_model("shared".into())
            .for_purpose(Purpose::Compaction);
        rank.begin(Billing::Api).unwrap();
        rank.attempt(false).unwrap();
        rank.attempt(true).unwrap();
        compact.begin(Billing::Api).unwrap();
        compact.attempt(false).unwrap();
        assert!(
            root.worker()
                .for_purpose(Purpose::Memory)
                .begin(Billing::Api)
                .is_err()
        );
        let next = root.next_turn(2).unwrap();
        rank.finish(
            Some(&Reply {
                prompt_tokens: Some(30),
                completion_tokens: Some(5),
                cached_prompt_tokens: Some(20),
                cache_creation_tokens: Some(3),
                reasoning_tokens: Some(2),
                ..Reply::default()
            }),
            Some(0.25),
            Billing::Api,
            2,
        )
        .unwrap();
        compact.finish(None, None, Billing::Api, 1).unwrap();
        let (total, turn) = root.snapshot();
        let ranked = turn.by_purpose[&Purpose::Retrieval];
        assert_eq!(ranked.requests, 1);
        assert_eq!(ranked.retries, 1);
        assert_eq!(ranked.unreported_attempts, 1);
        assert_eq!(ranked.cache_read, 20);
        assert_eq!(ranked.cache_write, 3);
        assert_eq!(ranked.reasoning, 2);
        assert_eq!(ranked.observed_cost_usd, Some(0.25));
        assert!(matches!(ranked.cost_coverage, CostCoverage::Partial));
        assert_eq!(turn.by_purpose[&Purpose::Compaction].failed_requests, 1);
        assert_eq!(turn.by_model["shared"].workers.requests, 2);
        let sum = turn
            .by_purpose
            .values()
            .try_fold(Counts::default(), |sum, &c| sum.add(c))
            .unwrap();
        assert_eq!(sum.requests, total.all.requests);
        assert_eq!(sum.input, total.all.input);
        assert_eq!(sum.output, total.all.output);
        assert_eq!(sum.observed_cost_usd, total.all.observed_cost_usd);
        assert!(next.snapshot().1.by_purpose.is_empty());
        assert_eq!(
            serde_json::to_value(&turn).unwrap()["by_purpose"]["retrieval"]["input"],
            30
        );
    }

    #[test]
    fn external_receipts_preserve_purpose_and_release_unstarted_reservations() {
        let root = Scope::default().next_turn(2).unwrap();
        let memory = root.worker().for_purpose(Purpose::Memory);
        drop(memory.reserve_external("memory-service".into()).unwrap());
        assert!(root.snapshot().0.by_purpose.is_empty());
        let mut received = memory.reserve_external("memory-service".into()).unwrap();
        received.mark_started().unwrap();
        let mut lost = memory.reserve_external("memory-service".into()).unwrap();
        lost.mark_started().unwrap();
        assert!(memory.reserve_external("memory-service".into()).is_err());
        let next = root.next_turn(2).unwrap();
        received
            .account(
                &serde_json::json!({
                    "prompt_tokens": 9, "completion_tokens": 4, "cost": 0.5
                }),
                true,
                true,
            )
            .unwrap();
        drop(lost);
        let total = next.snapshot().0;
        let observed = total.by_purpose[&Purpose::Memory];
        assert_eq!(observed.requests, 2);
        assert_eq!(observed.successful_requests, 1);
        assert_eq!(observed.failed_requests, 1);
        assert_eq!(observed.discarded_attempts, 2);
        assert_eq!(observed.reported_responses, 1);
        assert_eq!(observed.unreported_attempts, 1);
        assert_eq!(observed.observed_cost_usd, Some(0.5));
        assert!(matches!(observed.cost_coverage, CostCoverage::Partial));
        assert_eq!(root.snapshot().1.by_purpose[&Purpose::Memory].input, 9);
        assert!(next.snapshot().1.by_purpose.is_empty());
    }

    #[test]
    fn wrong_purpose_or_model_cannot_settle_another_scopes_transport_attempt() {
        let root = Scope::default()
            .next_turn(0)
            .unwrap()
            .for_model("model-a".into());
        root.begin(Billing::Api).unwrap();
        root.attempt(false).unwrap();
        let reply = Reply {
            prompt_tokens: Some(3),
            completion_tokens: Some(2),
            ..Reply::default()
        };
        let before = serde_json::to_value(root.snapshot().0).unwrap();
        for wrong in [
            root.for_purpose(Purpose::Repair),
            root.for_model("model-b".into()),
        ] {
            assert!(wrong.finish(Some(&reply), None, Billing::Api, 1).is_err());
            assert_eq!(serde_json::to_value(root.snapshot().0).unwrap(), before);
        }
        root.finish(Some(&reply), None, Billing::Api, 1).unwrap();
        assert_eq!(
            root.snapshot().0.by_purpose[&Purpose::Agent].unreported_attempts,
            0
        );
    }

    #[test]
    fn purpose_overflow_does_not_partially_update_any_group() {
        let root = Scope::default().next_turn(0).unwrap();
        {
            let mut state = root.state.lock().unwrap();
            state
                .total
                .by_purpose
                .entry(Purpose::Agent)
                .or_default()
                .requests = u64::MAX;
        }
        let before = serde_json::to_value(root.snapshot()).unwrap();
        assert!(root.begin(Billing::Api).is_err());
        assert_eq!(serde_json::to_value(root.snapshot()).unwrap(), before);
    }
}
