//! Runtime accounting of reported usage, shared across parent and worker backends.
use super::Reply;
use anyhow::{Result, ensure};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug, Default, Serialize)]
pub struct Counts {
    pub requests: u64,
    pub reported_responses: u64,
    pub failed_requests: u64,
    pub retries: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
}
impl Counts {
    fn finish(&mut self, reply: Option<&Reply>) {
        let Some(r) = reply else {
            self.failed_requests += 1;
            return;
        };
        self.reported_responses +=
            u64::from(r.prompt_tokens.is_some() && r.completion_tokens.is_some());
        self.input += r.prompt_tokens.unwrap_or(0);
        self.output += r.completion_tokens.unwrap_or(0);
        self.cache_read += r.cached_prompt_tokens.unwrap_or(0);
        self.cache_write += r.cache_creation_tokens.unwrap_or(0);
        self.reasoning += r.reasoning_tokens.unwrap_or(0);
    }
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct Groups {
    pub main: Counts,
    pub workers: Counts,
}
impl Groups {
    fn select(&mut self, worker: bool) -> &mut Counts {
        if worker {
            &mut self.workers
        } else {
            &mut self.main
        }
    }
}
#[derive(Default)]
struct State {
    next: u64,
    total: Groups,
    turns: BTreeMap<u64, Groups>,
}
#[derive(Clone, Default)]
pub struct Scope {
    state: Arc<Mutex<State>>,
    turn: u64,
    worker: bool,
    worker_limit: u64,
}
impl Scope {
    pub fn next_turn(&self, worker_limit: u32) -> Self {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.next += 1;
        let turn = state.next;
        state.turns.insert(turn, Groups::default());
        Self {
            state: self.state.clone(),
            turn,
            worker: false,
            worker_limit: u64::from(worker_limit),
        }
    }
    pub fn worker(&self) -> Self {
        Self {
            worker: true,
            ..self.clone()
        }
    }
    pub fn begin(&self) -> Result<()> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let turn = s.turns.entry(self.turn).or_default();
        ensure!(
            !self.worker || turn.workers.requests < self.worker_limit,
            "shared worker request budget exhausted for this turn"
        );
        turn.select(self.worker).requests += 1;
        s.total.select(self.worker).requests += 1;
        Ok(())
    }
    pub fn retry(&self) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.turns
            .entry(self.turn)
            .or_default()
            .select(self.worker)
            .retries += 1;
        s.total.select(self.worker).retries += 1;
    }
    pub fn finish(&self, reply: Option<&Reply>) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.turns
            .entry(self.turn)
            .or_default()
            .select(self.worker)
            .finish(reply);
        s.total.select(self.worker).finish(reply);
    }
    pub fn snapshot(&self) -> (Groups, Groups) {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (
            s.total.clone(),
            s.turns.get(&self.turn).cloned().unwrap_or_default(),
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn worker_budget_is_shared_and_late_usage_stays_in_its_original_turn() {
        let scope = Scope::default().next_turn(2);
        let worker = scope.worker();
        worker.begin().unwrap();
        worker.retry();
        let second = worker.clone();
        second.begin().unwrap();
        assert!(second.begin().is_err());
        let next = scope.next_turn(2);
        worker.finish(Some(&Reply {
            prompt_tokens: Some(12),
            completion_tokens: Some(3),
            ..Reply::default()
        }));
        second.finish(None);
        let (total, current) = next.snapshot();
        assert_eq!(total.workers.requests, 2);
        assert_eq!(total.workers.input, 12);
        assert_eq!(total.workers.retries, 1);
        assert_eq!(total.workers.reported_responses, 1);
        assert_eq!(total.workers.failed_requests, 1);
        assert_eq!(current.workers.requests, 0);
        assert_eq!(scope.snapshot().1.workers.input, 12);
    }
}
