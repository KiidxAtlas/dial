//! Experimental `streaming_dispatch`: start admitted read-only calls while the reply still streams.
//!
//! A streamed call is complete when the provider explicitly closes it, or moves to a later index.
//! Without a provider completion signal, the last call waits for the reply. Results are keyed by the exact
//! normalized call and are offered only to the ordinary parallel-read admission after the reply
//! ends, so a mis-detected or changed call simply runs as usual. Once any call in the reply runs
//! outside the parallel-read path, every remaining result is discarded as possibly stale.
use crate::agent::run::dispatch::image_question;
use crate::agent::*;
use std::collections::BTreeSet;

/// Streaming tool-argument observer driven by the chat loop.
pub(in crate::agent) trait ToolStream {
    /// One argument fragment for the tool call at `index`.
    fn arguments(&mut self, index: u64, id: &str, name: &str, delta: &str);
    /// An explicitly closed provider call; arguments replace accumulated fragments.
    fn completed(&mut self, index: u64, id: &str, name: &str, arguments: &str);
    /// A new attempt begins: partial calls from the discarded attempt never complete.
    fn restart(&mut self);
}

/// Owned execution context for early reads; nothing here reaches agent, session or UI state.
struct Runner {
    workspace: Workspace,
    config: Config,
    ext: Registry,
    interrupt: Arc<AtomicBool>,
}

/// Controller-side admission data, evaluated on the UI thread before a read starts.
pub(in crate::agent) struct Admission {
    pub(in crate::agent) hooks: Vec<crate::config::types::HookConfig>,
    pub(in crate::agent) always: Vec<crate::agent::safety::approval::PermissionRule>,
    pub(in crate::agent) schemas: Vec<Value>,
    pub(in crate::agent) names: Vec<String>,
    pub(in crate::agent) failed_calls: BTreeSet<[u8; 32]>,
}

#[derive(Default)]
struct Partial {
    id: String,
    name: String,
    arguments: String,
}

type Outcome = (ToolOutput, f64);

pub(in crate::agent) struct StreamingDispatch {
    runner: Arc<Runner>,
    admission: Admission,
    limit: usize,
    partial: BTreeMap<u64, Partial>,
    closed: BTreeSet<u64>,
    started: HashMap<String, Receiver<Outcome>>,
    clean: bool,
}

fn key(call: &ToolCall) -> String {
    format!(
        "{}\u{0}{}",
        call.name,
        crate::agent::safety::guard::normalized_arguments(call)
    )
}

impl StreamingDispatch {
    pub(in crate::agent) fn new(
        workspace: Workspace,
        config: Config,
        ext: Registry,
        interrupt: Arc<AtomicBool>,
        admission: Admission,
    ) -> Self {
        let limit = config.task.max_concurrency.max(1);
        Self {
            runner: Arc::new(Runner {
                workspace,
                config,
                ext,
                interrupt,
            }),
            admission,
            limit,
            partial: BTreeMap::new(),
            closed: BTreeSet::new(),
            started: HashMap::new(),
            clean: true,
        }
    }

    /// The same admission the parallel-read group applies, minus checks that need the live
    /// controller; those run again before any result is used.
    fn admits(&self, call: &ToolCall) -> bool {
        let runner = &self.runner;
        let admission = &self.admission;
        !admission
            .failed_calls
            .contains(&crate::agent::run::engine::failed_identity(call))
            && crate::agent::safety::approval::parallel_read(
                &runner.ext,
                &call.name,
                &call.arguments,
            )
            && ["pre_tool_use", "post_tool_use"].iter().all(|event| {
                !crate::agent::safety::hooks::wanted_for_tool(&admission.hooks, event, &call.name)
            })
            && !crate::agent::safety::approval::denied_by_rule(
                &admission.always,
                call,
                runner.workspace.root(),
            )
            && image_question(call).is_none()
            && call_form_error(call, &admission.schemas, &admission.names).is_none()
    }

    fn complete(&mut self, partial: Partial) {
        if self.started.len() >= self.limit || partial.name.is_empty() {
            return;
        }
        let arguments = if partial.arguments.trim().is_empty() {
            json!({})
        } else {
            match serde_json::from_str::<Value>(&partial.arguments) {
                Ok(value) if value.is_object() => value,
                _ => return,
            }
        };
        let call = ToolCall {
            id: partial.id,
            name: partial.name,
            arguments,
            malformed_arguments: false,
        };
        let key = key(&call);
        if self.started.contains_key(&key) || !self.admits(&call) {
            return;
        }
        let (tx, rx) = mpsc::sync_channel(1);
        let runner = self.runner.clone();
        let spawned = thread::Builder::new()
            .name("dial-streamed-read".into())
            .spawn(move || {
                let started = Instant::now();
                let signals = crate::tools::exec::signals::CallSignals::new();
                let interrupt = &*runner.interrupt;
                let output = crate::tools::exec::execution::run(interrupt, || {
                    crate::tools::exec::signals::scope(&signals, || {
                        if interrupt.load(Ordering::SeqCst) {
                            return interrupted_tool();
                        }
                        crate::tools::exec::execution::native_read(
                            &ToolContext {
                                workspace: &runner.workspace,
                                config: &runner.config,
                                mutation_authorizer: None,
                                confirm: None,
                            },
                            &runner.ext,
                            &call.name,
                            &call.arguments,
                            &|_| {},
                            interrupt,
                        )
                        .unwrap_or_else(|| ToolOutput::err("parallel tool unavailable"))
                    })
                });
                let _ = tx.send((output, started.elapsed().as_secs_f64()));
            });
        if spawned.is_ok() {
            self.started.insert(key, rx);
        }
    }

    /// The early result for exactly this call, waiting for it if it is still running.
    pub(in crate::agent) fn take(&mut self, call: &ToolCall, ui: &mut dyn Ui) -> Option<Outcome> {
        if !self.clean {
            return None;
        }
        let receiver = self.started.remove(&key(call))?;
        loop {
            match receiver.recv_timeout(Duration::from_millis(80)) {
                Ok(outcome) => return Some(outcome),
                Err(RecvTimeoutError::Timeout) => ui.heartbeat(),
                Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    /// A call ran outside the parallel-read path: later early results may be stale.
    pub(in crate::agent) fn invalidate(&mut self) {
        self.clean = false;
        self.started.clear();
        self.partial.clear();
    }

    #[cfg(test)]
    pub(in crate::agent) fn started(&self) -> usize {
        self.started.len()
    }
}

impl ToolStream for StreamingDispatch {
    fn arguments(&mut self, index: u64, id: &str, name: &str, delta: &str) {
        if !self.clean || self.closed.contains(&index) {
            return;
        }
        let earlier: Vec<u64> = self.partial.range(..index).map(|(&k, _)| k).collect();
        for done in earlier {
            self.closed.insert(done);
            if let Some(partial) = self.partial.remove(&done) {
                self.complete(partial);
            }
        }
        let entry = self.partial.entry(index).or_default();
        if !id.is_empty() {
            entry.id = id.to_owned();
        }
        if !name.is_empty() {
            entry.name = name.to_owned();
        }
        entry.arguments.push_str(delta);
    }

    fn completed(&mut self, index: u64, id: &str, name: &str, arguments: &str) {
        if !self.clean || !self.closed.insert(index) {
            return;
        }
        self.partial.remove(&index);
        self.complete(Partial {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: arguments.to_owned(),
        });
    }

    fn restart(&mut self) {
        // Started reads stay valid: they are keyed by the exact call and no tool ran meanwhile.
        self.partial.clear();
        self.closed.clear();
    }
}

#[cfg(test)]
mod tests;
