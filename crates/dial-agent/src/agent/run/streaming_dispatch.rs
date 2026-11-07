//! Experimental `streaming_dispatch`: start admitted read-only calls while the reply still streams.
//!
//! A streamed call is complete when the provider explicitly closes it, or moves to a later index.
//! Without a provider completion signal, the last call waits for the reply. Results are keyed by the exact
//! normalized call and are offered only to the ordinary parallel-read admission after the reply
//! ends, so a mis-detected or changed call simply runs as usual. When a call in the reply runs
//! outside the parallel-read path, results it may have changed are discarded as possibly stale:
//! bookkeeping and hook-free reads keep everything, a call with a resolved footprint drops only
//! conflicting results, and anything else discards the stream and cancels its in-flight reads.
use crate::agent::run::dependency_dispatch::{Footprint, bookkeeping, footprint_for};
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
    /// This stream's own cancellation: set when the stream is discarded or dropped, and mirrored
    /// from `interrupt` by a watcher while reads are in flight. Native reads observe only this.
    cancel: Arc<AtomicBool>,
    /// Coverage of earlier visible reads, frozen when the stream started (`ReadReuse`).
    reuse: crate::agent::workflow::investigation::ReadReuse,
}

impl Runner {
    /// Whether a delivered outcome was cut short by this stream rather than by the user.
    fn discarded(&self) -> bool {
        self.cancel.load(Ordering::SeqCst) && !self.interrupt.load(Ordering::SeqCst)
    }
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

/// One started read: its result channel and the workspace resources it observed.
struct Started {
    receiver: Receiver<Outcome>,
    footprint: Option<Footprint>,
}

pub(in crate::agent) struct StreamingDispatch {
    runner: Arc<Runner>,
    admission: Admission,
    limit: usize,
    partial: BTreeMap<u64, Partial>,
    closed: BTreeSet<u64>,
    started: HashMap<String, Started>,
    clean: bool,
    sealed: bool,
    started_count: u64,
    reused_count: Arc<std::sync::atomic::AtomicU64>,
    /// Ends the interrupt watcher when the stream is dropped.
    watching: Option<Arc<AtomicBool>>,
}

fn key(call: &ToolCall) -> String {
    format!(
        "{}\u{0}{}",
        call.name,
        crate::agent::safety::guard::normalized_arguments(call)
    )
}

/// Transferred to the existing worker pump. Never wait on it before launching sibling calls.
pub(in crate::agent) struct EarlyRead {
    receiver: Receiver<Outcome>,
    reused: Arc<std::sync::atomic::AtomicU64>,
    runner: Arc<Runner>,
}
impl EarlyRead {
    /// None when the stream cancelled this read; the caller then runs the call as usual.
    pub(in crate::agent) fn receive(self) -> Option<Outcome> {
        let outcome = self.receiver.recv().ok()?;
        if self.runner.discarded() {
            return None;
        }
        self.reused.fetch_add(1, Ordering::SeqCst);
        Some(outcome)
    }
}

impl StreamingDispatch {
    pub(in crate::agent) fn new(
        workspace: Workspace,
        config: Config,
        ext: Registry,
        interrupt: Arc<AtomicBool>,
        admission: Admission,
    ) -> Self {
        let limit = config.task.parallel_read_limit();
        Self {
            runner: Arc::new(Runner {
                workspace,
                config,
                ext,
                interrupt,
                cancel: Arc::new(AtomicBool::new(false)),
                reuse: Default::default(),
            }),
            admission,
            limit,
            partial: BTreeMap::new(),
            closed: BTreeSet::new(),
            started: HashMap::new(),
            clean: true,
            sealed: false,
            started_count: 0,
            reused_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            watching: None,
        }
    }

    /// Route early `read`s through the controller's frozen read coverage, as the ordinary
    /// parallel-read group does, so overlapping rereads return evidence links, not full text.
    pub(in crate::agent) fn with_reuse(
        mut self,
        reuse: crate::agent::workflow::investigation::ReadReuse,
    ) -> Self {
        if let Some(runner) = Arc::get_mut(&mut self.runner) {
            runner.reuse = reuse;
        }
        self
    }

    /// Mirror the global interrupt into this stream's cancel flag while it lives.
    fn watch(&mut self) {
        if self.watching.is_some() {
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let runner = self.runner.clone();
        let done = stop.clone();
        let spawned = thread::Builder::new()
            .name("dial-streamed-watch".into())
            .spawn(move || {
                while !done.load(Ordering::SeqCst) && !runner.cancel.load(Ordering::SeqCst) {
                    if runner.interrupt.load(Ordering::SeqCst) {
                        runner.cancel.store(true, Ordering::SeqCst);
                        break;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            });
        if spawned.is_ok() {
            self.watching = Some(stop);
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

    fn complete_if(&mut self, partial: Partial, authorize: &mut dyn FnMut(&ToolCall) -> bool) {
        if self.sealed || self.started.len() >= self.limit || partial.name.is_empty() {
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
        if !crate::agent::safety::approval::parallel_read(
            &self.runner.ext,
            &call.name,
            &call.arguments,
        ) {
            // Bookkeeping settles on the controller and observes no workspace state, so reads
            // after it stay valid; any other effect seals the read prefix.
            if !bookkeeping(&call) {
                self.sealed = true;
            }
            return;
        }
        if self.started.contains_key(&key(&call)) || !self.admits(&call) || !authorize(&call) {
            return;
        }
        let footprint = footprint_for(
            &self.runner.workspace,
            self.runner.workspace.edit_mode(),
            &self.runner.ext,
            &call,
        );
        self.spawn(call, footprint);
    }

    /// Start one admitted read on its own thread; its result waits in the stream until the
    /// controller's parallel-read group extracts it.
    fn spawn(&mut self, call: ToolCall, footprint: Option<Footprint>) -> bool {
        let key = key(&call);
        self.watch();
        let (tx, rx) = mpsc::sync_channel(1);
        let runner = self.runner.clone();
        let spawned = thread::Builder::new()
            .name("dial-streamed-read".into())
            .spawn(move || {
                let started = Instant::now();
                let signals = crate::tools::exec::signals::CallSignals::new();
                let cancel = &*runner.cancel;
                let output = crate::tools::exec::execution::run(cancel, || {
                    crate::tools::exec::signals::scope(&signals, || {
                        if cancel.load(Ordering::SeqCst) || runner.interrupt.load(Ordering::SeqCst)
                        {
                            return interrupted_tool();
                        }
                        let read = |args: &Value| {
                            crate::tools::exec::execution::native_read(
                                &ToolContext {
                                    workspace: &runner.workspace,
                                    config: &runner.config,
                                    mutation_authorizer: None,
                                    confirm: None,
                                },
                                &runner.ext,
                                &call.name,
                                args,
                                &|_| {},
                                cancel,
                            )
                            .unwrap_or_else(|| ToolOutput::err("parallel tool unavailable"))
                        };
                        // Admission already excluded hooked calls, as the parallel group requires.
                        if call.name == "read" {
                            runner
                                .reuse
                                .execute(&runner.workspace, &call.arguments, read)
                        } else {
                            read(&call.arguments)
                        }
                    })
                });
                let _ = tx.send((output, started.elapsed().as_secs_f64()));
            });
        if spawned.is_ok() {
            self.started.insert(
                key,
                Started {
                    receiver: rx,
                    footprint,
                },
            );
            self.started_count += 1;
        }
        spawned.is_ok()
    }

    /// Overlap an approval prompt with later reads. `barriers` are the resolved, hook-free
    /// footprints of the calls about to run (a serial call or a native wave), at least one of which
    /// waits for a human first; none has run. Start the admitted reads among `later` (up to the
    /// next effect; bookkeeping is skipped) whose footprints are disjoint from all of them. Their results stay in the stream, used only when the controller reaches
    /// those calls after the barrier settles ([`settled_beside`](Self::settled_beside) drops any
    /// the barrier's actual effects reached). A denied barrier never runs, so its reads stay valid.
    /// Returns how many reads started.
    pub(in crate::agent) fn start_beside(
        &mut self,
        barriers: &[Footprint],
        later: &[ToolCall],
        authorize: &mut dyn FnMut(&ToolCall) -> bool,
    ) -> usize {
        if !self.clean {
            return 0;
        }
        let mut started = 0;
        for call in later {
            if self.started.len() >= self.limit {
                break;
            }
            if !crate::agent::safety::approval::parallel_read(
                &self.runner.ext,
                &call.name,
                &call.arguments,
            ) {
                if bookkeeping(call) {
                    continue;
                }
                break;
            }
            if self.started.contains_key(&key(call)) || !self.admits(call) {
                continue;
            }
            let Some(footprint) = footprint_for(
                &self.runner.workspace,
                self.runner.workspace.edit_mode(),
                &self.runner.ext,
                call,
            ) else {
                continue;
            };
            if barriers.iter().any(|barrier| barrier.conflicts(&footprint)) || !authorize(call) {
                continue;
            }
            if self.spawn(call.clone(), Some(footprint)) {
                started += 1;
            }
        }
        started
    }

    /// The barrier [`start_beside`](Self::start_beside) overlapped has settled with `output`.
    /// Drop early results whose resources its recorded changes reached (they re-run in order);
    /// unknown effects discard the stream. A denied or failed call changed nothing.
    pub(in crate::agent) fn settled_beside(&mut self, output: &ToolOutput) {
        if !self.clean {
            return;
        }
        if output.execution.effects == crate::runtime::execution::Effects::Unknown {
            self.invalidate();
            return;
        }
        let changed = &output.execution.changed_paths;
        if changed.is_empty() {
            return;
        }
        let root = self.runner.workspace.root().to_owned();
        self.started.retain(|_, started| {
            started
                .footprint
                .as_ref()
                .is_some_and(|read| !read.touches(&root, changed))
        });
    }

    pub(in crate::agent) fn has(&self, call: &ToolCall) -> bool {
        self.clean && self.started.contains_key(&key(call))
    }

    pub(in crate::agent) fn extract(&mut self, call: &ToolCall) -> Option<EarlyRead> {
        if !self.clean {
            return None;
        }
        self.started.remove(&key(call)).map(|started| EarlyRead {
            receiver: started.receiver,
            reused: self.reused_count.clone(),
            runner: self.runner.clone(),
        })
    }

    #[cfg(test)]
    pub(in crate::agent) fn take(&mut self, call: &ToolCall, _ui: &mut dyn Ui) -> Option<Outcome> {
        self.extract(call)?.receive()
    }

    pub(in crate::agent) fn statistics(&self) -> (u64, u64) {
        (self.started_count, self.reused_count.load(Ordering::SeqCst))
    }

    /// Discard every early result and cancel the reads still running.
    pub(in crate::agent) fn invalidate(&mut self) {
        self.clean = false;
        self.started.clear();
        self.partial.clear();
        self.runner.cancel.store(true, Ordering::SeqCst);
    }

    /// `call` ran outside the parallel-read path with `hooked` lifecycle hooks: keep only the
    /// early results it cannot have changed. Bookkeeping and hook-free parallel reads change no
    /// workspace state; a resolved, hook-free footprint without verification drops conflicting
    /// (or unresolved) results only; anything else discards the stream.
    pub(in crate::agent) fn ran(
        &mut self,
        call: &ToolCall,
        hooked: bool,
        footprint: Option<Footprint>,
    ) {
        if !self.clean {
            return;
        }
        let verifies = call.arguments["verify"]
            .as_array()
            .is_some_and(|entries| !entries.is_empty());
        if bookkeeping(call)
            || (!hooked
                && crate::agent::safety::approval::parallel_read(
                    &self.runner.ext,
                    &call.name,
                    &call.arguments,
                ))
        {
            return;
        }
        match footprint {
            Some(footprint) if !hooked && !verifies => self.retain_disjoint(&[footprint]),
            _ => self.invalidate(),
        }
    }

    /// A native wave with these admitted footprints ran: drop results that may overlap it.
    pub(in crate::agent) fn retain_disjoint(&mut self, footprints: &[Footprint]) {
        self.started.retain(|_, started| {
            started
                .footprint
                .as_ref()
                .is_some_and(|read| footprints.iter().all(|ran| !ran.conflicts(read)))
        });
    }

    #[cfg(test)]
    pub(in crate::agent) fn started(&self) -> usize {
        self.started.len()
    }

    #[cfg(test)]
    pub(in crate::agent) fn cancelled(&self) -> bool {
        self.runner.cancel.load(Ordering::SeqCst)
    }
}

impl Drop for StreamingDispatch {
    /// Unconsumed early reads are unwanted once the round ends; extracted reads already settled.
    fn drop(&mut self) {
        if !self.started.is_empty() {
            self.runner.cancel.store(true, Ordering::SeqCst);
        }
        if let Some(stop) = &self.watching {
            stop.store(true, Ordering::SeqCst);
        }
    }
}

impl StreamingDispatch {
    pub(in crate::agent) fn arguments_if(
        &mut self,
        index: u64,
        id: &str,
        name: &str,
        delta: &str,
        authorize: &mut dyn FnMut(&ToolCall) -> bool,
    ) {
        if !self.clean || self.closed.contains(&index) {
            return;
        }
        let earlier: Vec<u64> = self.partial.range(..index).map(|(&k, _)| k).collect();
        for done in earlier {
            self.closed.insert(done);
            if let Some(partial) = self.partial.remove(&done) {
                self.complete_if(partial, authorize);
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

    pub(in crate::agent) fn completed_if(
        &mut self,
        index: u64,
        id: &str,
        name: &str,
        arguments: &str,
        authorize: &mut dyn FnMut(&ToolCall) -> bool,
    ) {
        if !self.clean || !self.closed.insert(index) {
            return;
        }
        // A later explicit completion is also the next-index signal for buffered earlier calls.
        // In particular, never start a read past an already observed mutation.
        let earlier: Vec<_> = self
            .partial
            .range(..index)
            .map(|(&index, _)| index)
            .collect();
        for done in earlier {
            self.closed.insert(done);
            if let Some(partial) = self.partial.remove(&done) {
                self.complete_if(partial, authorize);
            }
        }
        self.partial.remove(&index);
        self.complete_if(
            Partial {
                id: id.to_owned(),
                name: name.to_owned(),
                arguments: arguments.to_owned(),
            },
            authorize,
        );
    }

    pub(in crate::agent) fn restart_reads(&mut self) {
        // Started reads stay valid: they are keyed by the exact call and no tool ran meanwhile.
        self.partial.clear();
        self.closed.clear();
        self.sealed = false;
    }
}

impl ToolStream for StreamingDispatch {
    fn arguments(&mut self, index: u64, id: &str, name: &str, delta: &str) {
        self.arguments_if(index, id, name, delta, &mut |_| true);
    }
    fn completed(&mut self, index: u64, id: &str, name: &str, args: &str) {
        self.completed_if(index, id, name, args, &mut |_| true);
    }
    fn restart(&mut self) {
        self.restart_reads();
    }
}

#[cfg(test)]
mod tests;
