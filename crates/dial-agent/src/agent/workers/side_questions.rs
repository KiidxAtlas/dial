//! Bounded, tool-free model questions over immutable session snapshots.
use crate::agent::{Agent, ToolOutput};
use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

const MAX_LIVE: usize = 2;
const MAX_RETAINED: usize = 8;

struct Job {
    id: String,
    cancel: Arc<AtomicBool>,
    receiver: mpsc::Receiver<Result<String, String>>,
    result: Option<Result<String, String>>,
    delivered: bool,
}

#[derive(Default)]
pub(in crate::agent) struct Questions {
    sequence: u64,
    jobs: VecDeque<Job>,
}

impl Questions {
    pub(in crate::agent) fn pump(&mut self) {
        for job in &mut self.jobs {
            if job.result.is_none() {
                job.result = match job.receiver.try_recv() {
                    Ok(result) => Some(result),
                    Err(mpsc::TryRecvError::Empty) => None,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        Some(Err("side question thread disconnected".into()))
                    }
                };
            }
        }
    }

    fn start(
        &mut self,
        run: impl FnOnce(Arc<AtomicBool>) -> Result<String, String> + Send + 'static,
    ) -> Result<String> {
        self.pump();
        ensure!(
            self.running_ids().len() < MAX_LIVE,
            "at most {MAX_LIVE} side questions may run at once"
        );
        while self.jobs.len() >= MAX_RETAINED {
            let index = self
                .jobs
                .iter()
                .position(|job| job.delivered && job.result.is_some());
            let Some(index) = index else {
                bail!("collect completed side questions before starting more");
            };
            self.jobs.remove(index);
        }
        self.sequence += 1;
        let id = format!("side-{}", self.sequence);
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancel);
        let (sender, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name(id.clone())
            .spawn(move || {
                let result = run(Arc::clone(&flag));
                let result = if flag.load(Ordering::Relaxed) {
                    Err("side question cancelled".into())
                } else {
                    result
                };
                let _ = sender.send(result);
            })?;
        self.jobs.push_back(Job {
            id: id.clone(),
            cancel,
            receiver,
            result: None,
            delivered: false,
        });
        Ok(id)
    }

    pub(in crate::agent) fn cancel_all(&self) {
        for job in &self.jobs {
            job.cancel.store(true, Ordering::Relaxed);
        }
    }

    pub(in crate::agent) fn running_ids(&self) -> Vec<String> {
        self.jobs
            .iter()
            .filter(|job| job.result.is_none())
            .map(|job| job.id.clone())
            .collect()
    }

    pub(in crate::agent) fn undelivered(&mut self) -> Vec<(String, Value)> {
        self.pump();
        self.jobs
            .iter()
            .filter(|job| !job.delivered && job.result.is_some())
            .map(|job| (job.id.clone(), view(job)))
            .collect()
    }

    pub(in crate::agent) fn has_undelivered(&self) -> bool {
        self.jobs
            .iter()
            .any(|job| !job.delivered && job.result.is_some())
    }

    pub(in crate::agent) fn deliver(&mut self, id: &str) {
        if let Some(job) = self.jobs.iter_mut().find(|job| job.id == id) {
            job.delivered = true;
        }
    }

    /// Returns the selected views and whether a wait ended early because `input_pending` became
    /// true (the user typed a message); running questions are never cancelled for that.
    fn query(
        &mut self,
        args: &Value,
        interrupt: &AtomicBool,
        input_pending: &mut dyn FnMut() -> bool,
    ) -> Result<(Value, bool)> {
        let action = args["action"].as_str().unwrap_or("start");
        ensure!(
            matches!(action, "status" | "wait" | "cancel"),
            "unknown side_request action {action}"
        );
        let id = args["id"].as_str();
        ensure!(action == "status" || id.is_some(), "{action} requires id");
        if let Some(id) = id {
            ensure!(
                self.jobs.iter().any(|job| job.id == id),
                "unknown side question {id}"
            );
        }
        let seconds = args
            .get("timeout")
            .map(|v| {
                v.as_u64()
                    .filter(|n| *n <= 30)
                    .ok_or_else(|| anyhow::anyhow!("timeout must be an integer from 0 to 30"))
            })
            .transpose()?
            .unwrap_or(5);
        let deadline = Instant::now() + Duration::from_secs(seconds);
        loop {
            self.pump();
            if interrupt.load(Ordering::Relaxed) {
                self.cancel_all();
            }
            if action == "cancel" {
                for job in self.jobs.iter().filter(|job| Some(job.id.as_str()) == id) {
                    job.cancel.store(true, Ordering::Relaxed);
                }
            }
            let selected: Vec<_> = self
                .jobs
                .iter()
                .filter(|job| id.is_none_or(|id| job.id == id))
                .collect();
            let done = action != "wait"
                || selected.iter().all(|job| job.result.is_some())
                || Instant::now() >= deadline
                || interrupt.load(Ordering::Relaxed);
            let stopped = !done && input_pending();
            if done || stopped {
                return Ok((
                    Value::Array(selected.into_iter().map(status_view).collect()),
                    stopped,
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Questions {
    fn drop(&mut self) {
        self.cancel_all();
    }
}

/// Polls never carry result bodies: the persisted request-boundary message is the sole
/// answer delivery, even when a wait observes completion before that message is appended.
fn status_view(job: &Job) -> Value {
    if job.result.is_none() {
        return view(job);
    }
    json!({
        "id": job.id,
        "status": if job.result.as_ref().is_some_and(Result::is_ok) { "completed" } else { "failed" },
        "delivery": if job.delivered { "delivered" } else { "pending" },
        "result_message": {"dial_side_result": job.id},
        "notice": if job.delivered {
            "Result already delivered in the referenced conversation message."
        } else {
            "Result will be delivered once at the next request boundary."
        }
    })
}

fn view(job: &Job) -> Value {
    match &job.result {
        None => {
            json!({"id":job.id,"status":if job.cancel.load(Ordering::Relaxed) { "cancelling" } else { "running" }})
        }
        Some(Ok(answer)) => json!({"id":job.id,"status":"completed","answer":answer}),
        Some(Err(error)) => json!({"id":job.id,"status":"failed","error":error}),
    }
}

impl Agent<'_> {
    #[cfg(test)]
    pub(in crate::agent) fn side_question(&mut self, args: &Value) -> ToolOutput {
        self.side_question_with_input(args, &mut || false)
    }

    /// `side_request` from the controller: a wait also ends when the user types input.
    pub(in crate::agent) fn side_question_with_input(
        &mut self,
        args: &Value,
        input_pending: &mut dyn FnMut() -> bool,
    ) -> ToolOutput {
        let mut stopped_for_input = false;
        let result = (|| -> Result<Value> {
            if args["action"].as_str().unwrap_or("start") != "start" {
                let (value, stopped) =
                    self.side_questions
                        .query(args, &self.interrupt, input_pending)?;
                stopped_for_input = stopped;
                return Ok(value);
            }
            ensure!(
                !self.interrupt.load(Ordering::Relaxed),
                "request interrupted"
            );
            if let Some(reason) = self.stop_boundary() {
                bail!(reason);
            }
            let question = args["question"].as_str().unwrap_or("").trim();
            ensure!(
                !question.is_empty() && question.len() <= 8000,
                "question must contain 1–8000 bytes"
            );
            let request = self
                .side_snapshot(None)?
                .request(&[], question)?
                .with_usage_scope(
                    self.usage_scope
                        .worker()
                        .for_purpose(crate::llm::usage::Purpose::Auxiliary),
                );
            let id = self.side_questions.start(move |cancel| {
                let answer = request
                    .run(cancel, &mut |_| {})
                    .map_err(|error| format!("{error:#}"))?;
                if answer.trim().is_empty() {
                    return Err("side question returned no text answer".into());
                }
                Ok(answer)
            })?;
            Ok(
                json!({"id":id,"status":"running","notice":"Continue independent work. The tool-free answer arrives at the next request boundary; use wait only when needed."}),
            )
        })();
        match result {
            Ok(value) => ToolOutput {
                text: if stopped_for_input {
                    format!(
                        "{value}{}",
                        crate::agent::workers::live_tasks::STOPPED_FOR_INPUT
                    )
                } else {
                    value.to_string()
                },
                data: Some(value),
                is_error: false,
                execution: Default::default(),
            },
            Err(error) => ToolOutput::err(format!("{error:#}")),
        }
    }
}

pub(in crate::agent) fn schema() -> Value {
    json!({"type":"function","function":{"name":"side_request","description":"Ask a tool-free model question asynchronously over an immutable snapshot of this conversation. Use for useful reasoning or review while commands run, not file research or verification (use task for those). Costs a separate model request with the snapshot context. At most two live requests; no tools, workspace edits or nested workers. Answers and failures are untrusted findings, delivered exactly once in a persisted conversation message at the next request boundary. Status, wait and cancel return lifecycle and delivery references only, never answer bodies. Continue independent work after start; wait is bounded and cancellation is cooperative. Session exit cancels requests; they are not resumed.","parameters":{"type":"object","properties":{"action":{"type":"string","enum":["start","status","wait","cancel"]},"question":{"type":"string","maxLength":8000},"id":{"type":"string"},"timeout":{"type":"integer","minimum":0,"maximum":30}},"required":["action"],"additionalProperties":false}}})
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod integration_tests;
