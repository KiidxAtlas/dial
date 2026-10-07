//! Versioned newline-delimited JSON control protocol. Stdin is control, stdout is JSON only.
use crate::{
    agent::{Agent, Decision, Ui, make_backend, todo::TodoList},
    config::types::Config,
    storage::{
        paths::Layout,
        session::{Session, SessionMeta},
    },
    tools::ToolOutput,
};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    io::{BufRead, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

pub const PROTOCOL_VERSION: u32 = 1;
const MAX_REQUEST: usize = 1024 * 1024;
#[derive(Clone)]
struct Sender(Arc<Mutex<std::io::Stdout>>);
impl Sender {
    fn emit(&self, value: Value) {
        if let Ok(mut out) = self.0.lock() {
            let _ = serde_json::to_writer(&mut *out, &value);
            let _ = out.write_all(b"\n");
            let _ = out.flush();
        }
    }
    fn result(&self, id: &Value, result: Result<Value>) {
        self.emit(match result {
            Ok(v) => json!({"id":id,"result":v}),
            Err(e) => json!({"id":id,"error":{"message":format!("{e:#}")}}),
        });
    }
}
struct RpcUi {
    out: Sender,
    approvals: mpsc::Receiver<(String, Decision)>,
    pending_approval: Arc<Mutex<Option<String>>>,
    interrupt: Arc<AtomicBool>,
    steering: Arc<Mutex<Vec<String>>>,
    next: u64,
}
impl RpcUi {
    fn request_approval(
        &mut self,
        tool: &str,
        summary: &str,
        cancel: Option<&AtomicBool>,
    ) -> Decision {
        self.next += 1;
        let approval_id = format!("approval-{}", self.next);
        if let Ok(mut pending) = self.pending_approval.lock() {
            *pending = Some(approval_id.clone());
        } else {
            return Decision::Deny;
        }
        self.out.emit(
            json!({"event":"approval","approval_id":approval_id,"tool":tool,"summary":summary}),
        );
        let decision = loop {
            if self.interrupt.load(Ordering::SeqCst)
                || cancel.is_some_and(|flag| flag.load(Ordering::SeqCst))
            {
                break Decision::Deny;
            }
            match self.approvals.recv_timeout(Duration::from_millis(100)) {
                Ok((id, d)) if id == approval_id => break d,
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => break Decision::Deny,
            }
        };
        if let Ok(mut pending) = self.pending_approval.lock()
            && pending.as_deref() == Some(&approval_id)
        {
            *pending = None;
        }
        decision
    }
}
impl Ui for RpcUi {
    fn text_delta(&mut self, text: &str) {
        self.out.emit(json!({"event":"text","delta":text}));
    }
    fn tool_call(&mut self, name: &str, summary: &str) {
        self.out
            .emit(json!({"event":"tool_call","name":name,"summary":summary}));
    }
    fn tool_result(&mut self, name: &str, output: &ToolOutput) {
        self.out.emit(json!({"event":"tool_result","name":name,"text":crate::agent::guard::clip_middle(&output.text,24*1024),"is_error":output.is_error}));
    }
    fn notice(&mut self, message: &str) {
        self.out.emit(json!({"event":"notice","message":message}));
    }
    fn context_usage(&mut self, tokens: u64, estimated: bool) {
        self.out
            .emit(json!({"event":"context_usage","tokens":tokens,"estimated":estimated}));
    }
    fn usage(&mut self, prompt: Option<u64>, output: Option<u64>, seconds: f64) {
        self.out.emit(json!({"event":"usage","prompt_tokens":prompt,"output_tokens":output,"seconds":seconds}));
    }
    fn tool_timing(&mut self, name: &str, seconds: f64) {
        self.out
            .emit(json!({"event":"tool_timing","name":name,"seconds":seconds}));
    }
    fn tool_output(&mut self, name: &str, text: &str) {
        self.out
            .emit(json!({"event":"tool_output","name":name,"delta":text}));
    }
    fn background_notice(&mut self, message: &str) {
        self.out
            .emit(json!({"event":"background","message":message}));
    }
    fn reasoning_delta(&mut self, text: &str) {
        self.out.emit(json!({"event":"reasoning","delta":text}));
    }
    fn tool_arguments_delta(&mut self, index: u64, id: &str, name: &str, delta: &str) {
        self.out.emit(
            json!({"event":"tool_arguments","index":index,"id":id,"name":name,"delta":delta}),
        );
    }
    fn cached_usage(&mut self, read: Option<u64>, write: Option<u64>) {
        self.out
            .emit(json!({"event":"cached_usage","read_tokens":read,"write_tokens":write}));
    }
    fn extension_panel(&mut self, title: &str, markdown: &str) {
        self.out
            .emit(json!({"event":"panel","title":title,"markdown":markdown}));
    }
    fn todo(&mut self, todo: &TodoList) {
        self.out
            .emit(json!({"event":"todo","tasks":todo.snapshot()}));
    }
    fn take_steering(&mut self) -> Vec<String> {
        self.steering
            .lock()
            .map(|mut q| std::mem::take(&mut *q))
            .unwrap_or_default()
    }
    fn approve(&mut self, tool: &str, summary: &str) -> Decision {
        self.request_approval(tool, summary, None)
    }
    fn approve_with_cancel(&mut self, tool: &str, summary: &str, cancel: &AtomicBool) -> Decision {
        self.request_approval(tool, summary, Some(cancel))
    }
}

fn read_request(
    reader: &mut impl BufRead,
) -> std::io::Result<Option<Result<Vec<u8>, &'static str>>> {
    let mut bytes = Vec::new();
    let mut oversized = false;
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(if bytes.is_empty() && !oversized {
                None
            } else if oversized {
                Some(Err("RPC request exceeds 1 MiB"))
            } else {
                Some(Ok(bytes))
            });
        }
        let used = buffer
            .iter()
            .position(|b| *b == b'\n')
            .map_or(buffer.len(), |at| at + 1);
        let ended = buffer[used - 1] == b'\n';
        if !oversized {
            if bytes.len().saturating_add(used) > MAX_REQUEST + 1 {
                oversized = true;
                bytes.clear();
            } else {
                bytes.extend_from_slice(&buffer[..used]);
            }
        }
        reader.consume(used);
        if ended {
            if oversized {
                return Ok(Some(Err("RPC request exceeds 1 MiB")));
            }
            bytes.pop();
            return Ok(Some(Ok(bytes)));
        }
    }
}

/// Run a persistent agent. Control requests received during inference can abort, steer or approve.
pub fn run(layout: Layout, config: Config) -> Result<()> {
    config.require_model()?;
    layout.ensure_state_outside_project()?;
    let out = Sender(Arc::new(Mutex::new(std::io::stdout())));
    let interrupt = Arc::new(AtomicBool::new(false));
    let busy = Arc::new(AtomicBool::new(false));
    let steering = Arc::new(Mutex::new(Vec::new()));
    let (commands, rx) = mpsc::sync_channel::<Value>(8);
    let (approval_tx, approval_rx) = mpsc::channel();
    let pending_approval = Arc::new(Mutex::new(None::<String>));
    let worker_pending = pending_approval.clone();
    let (control_tx, control_rx) = mpsc::sync_channel(1);
    let worker_out = out.clone();
    let flag = interrupt.clone();
    let active = busy.clone();
    let queue = steering.clone();
    let worker = thread::spawn(move || -> Result<()> {
        let outcome = (|| -> Result<()> {
            let mut compute = None;
            let backend = make_backend(&config)?;
            let session = Session::create(
                &layout,
                SessionMeta {
                    model: config.model.clone(),
                },
            )?;
            let mut agent = Agent::interactive(
                &config,
                backend,
                layout.clone(),
                session,
                None,
                None,
                flag.clone(),
            )?;
            if let Some(path) = std::env::var_os("DIAL_BENCHMARK_ADAPTER") {
                agent.set_benchmark_adapter(std::path::Path::new(&path))?;
            }
            let _ = control_tx.send(agent.live_task_control());
            let mut ui = RpcUi {
                out: worker_out.clone(),
                approvals: approval_rx,
                pending_approval: worker_pending,
                interrupt: flag.clone(),
                steering: queue,
                next: 0,
            };
            worker_out.emit(json!({"event":"ready","protocol_version":PROTOCOL_VERSION,"session_id":agent.session().id(),"model":config.model,"tools":agent.tool_names()}));
            loop {
                let request = match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(request) => request,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        agent.pump_subagents(&mut ui);
                        if compute.is_some()
                            && (crate::learn::compute::background_pending(&layout.user_dir)?
                                || crate::learn::compute::foreground_pending(&layout.user_dir)?)
                            && agent.release_idle_models()
                            && let Some(lock) = compute.take()
                        {
                            crate::learn::compute::clear_owner(&layout.user_dir);
                            fs2::FileExt::unlock(&lock)?;
                        }
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                let id = request.get("id").cloned().unwrap_or(Value::Null);
                let params = &request["params"];
                if config.uses_local_compute()
                    && compute.is_none()
                    && matches!(
                        request["method"].as_str(),
                        Some("send" | "compact" | "task_start")
                    )
                {
                    compute = Some(crate::learn::auto::compute_lock(&layout)?);
                    crate::learn::compute::register(
                        &layout.user_dir,
                        "session",
                        &layout.project_root,
                        &config.state_dir,
                        &config.model,
                    )?;
                }
                let result = (|| -> Result<Value> {
                    match request["method"].as_str().unwrap_or("") {
                        "send" => {
                            let text = params["text"]
                                .as_str()
                                .context("send requires params.text")?;
                            let started = Instant::now();
                            let report = agent.turn_preserving_interrupt(text, &mut ui)?;
                            Ok(
                                json!({"text":report.text,"outcome":report.outcome,"steps":report.steps,"interrupted":report.interrupted,"duration_ms":started.elapsed().as_millis(),"session_id":agent.session().id(),"metrics":agent.metrics()}),
                            )
                        }
                        "state" => Ok(
                            json!({"session_id":agent.session().id(),"title":agent.session().title(),"tools":agent.tool_names(),"plan_mode":agent.plan_mode(),"cost":agent.session_cost(),"metrics":agent.metrics()}),
                        ),
                        "plan" => {
                            agent.set_plan_mode(
                                params["enabled"]
                                    .as_bool()
                                    .context("plan requires params.enabled")?,
                            );
                            Ok(json!({"enabled":agent.plan_mode()}))
                        }
                        "new_session" => {
                            agent.swap_session(Session::create(
                                &layout,
                                SessionMeta {
                                    model: config.model.clone(),
                                },
                            )?);
                            Ok(json!({"session_id":agent.session().id()}))
                        }
                        "compact" => {
                            agent.compact(&mut ui)?;
                            Ok(json!({"compacted":true}))
                        }
                        "seed_context" => {
                            let messages = params["messages"]
                                .as_array()
                                .context("seed_context requires params.messages")?;
                            anyhow::ensure!(messages.len() <= 200, "at most 200 seed messages");
                            for text in messages {
                                let text =
                                    text.as_str().context("context entries must be strings")?;
                                anyhow::ensure!(
                                    text.len() <= 1024 * 1024,
                                    "context entry exceeds 1 MiB"
                                );
                                agent
                                    .session_mut()
                                    .append(json!({"role":"user","content":text}))?;
                            }
                            Ok(json!({"seeded":messages.len()}))
                        }
                        "resume" => {
                            let wanted = params["session_id"]
                                .as_str()
                                .context("resume requires params.session_id")?;
                            let sessions = Session::list(&layout, usize::MAX)?;
                            let matching: Vec<_> = sessions
                                .iter()
                                .filter(|s| s.id.starts_with(wanted))
                                .collect();
                            anyhow::ensure!(matching.len() == 1, "session id missing or ambiguous");
                            agent.swap_session(Session::open(&matching[0].path)?);
                            Ok(json!({"session_id":agent.session().id()}))
                        }
                        "messages" => Ok(json!({"messages":agent.session().messages()})),
                        "rename" => {
                            agent.session_mut().rename(
                                params["title"]
                                    .as_str()
                                    .context("rename requires params.title")?,
                            )?;
                            Ok(json!({"title":agent.session().title()}))
                        }
                        "task_start" => agent.start_subagents(params, &mut ui),
                        "task_status" => agent.subagent_status(params["id"].as_str(), &mut ui),
                        _ => anyhow::bail!("unknown RPC method"),
                    }
                })();
                active.store(false, Ordering::SeqCst);
                worker_out.result(&id, result);
            }
            Ok(())
        })();
        if let Err(error) = &outcome {
            worker_out.emit(json!({"event":"fatal","message":format!("{error:#}")}));
        }
        outcome
    });
    let task_control = control_rx.recv().context("RPC initialization failed")?;
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    while let Some(line) = read_request(&mut input)? {
        let line = match line {
            Ok(line) => line,
            Err(message) => {
                out.result(&Value::Null, Err(anyhow::anyhow!(message)));
                continue;
            }
        };
        if line.is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_slice(&line) {
            Ok(v) => v,
            Err(e) => {
                out.result(&Value::Null, Err(e.into()));
                continue;
            }
        };
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        match request["method"].as_str().unwrap_or("") {
            "abort" => {
                interrupt.store(true, Ordering::SeqCst);
                out.result(&id, Ok(json!({"requested":true})));
            }
            "steer" => {
                let result = (|| -> Result<Value> {
                    anyhow::ensure!(busy.load(Ordering::SeqCst), "no active turn");
                    let text = request["params"]["text"]
                        .as_str()
                        .context("steer requires params.text")?;
                    let mut q = steering
                        .lock()
                        .map_err(|_| anyhow::anyhow!("steering unavailable"))?;
                    anyhow::ensure!(q.len() < 8, "steering queue full");
                    q.push(text.to_owned());
                    Ok(json!({"queued":true}))
                })();
                out.result(&id, result);
            }
            "approve" => {
                let result = (|| -> Result<Value> {
                    let approval_id = request["params"]["approval_id"]
                        .as_str()
                        .context("approval_id required")?;
                    let decision = match request["params"]["decision"].as_str() {
                        Some("allow") => Decision::Allow,
                        Some("always") => Decision::AllowAlways,
                        Some("deny") => Decision::Deny,
                        _ => anyhow::bail!("decision must be allow, always or deny"),
                    };
                    let mut pending = pending_approval
                        .lock()
                        .map_err(|_| anyhow::anyhow!("approval unavailable"))?;
                    anyhow::ensure!(
                        pending.as_deref() == Some(approval_id),
                        "approval id is not currently pending"
                    );
                    *pending = None;
                    approval_tx.send((approval_id.to_owned(), decision))?;
                    Ok(json!({"received":true}))
                })();
                out.result(&id, result);
            }
            "shutdown" => {
                interrupt.store(true, Ordering::SeqCst);
                out.result(&id, Ok(json!({"closing":true})));
                break;
            }
            "task_cancel" => {
                let result = request["params"]["id"]
                    .as_str()
                    .context("task id required")
                    .and_then(|id| task_control.cancel(id));
                out.result(&id, result);
            }
            "task_message" => {
                let result = (|| -> Result<Value> {
                    let job = request["params"]["id"]
                        .as_str()
                        .context("task id required")?;
                    let text = request["params"]["text"]
                        .as_str()
                        .context("text required")?;
                    task_control.send(job, text)
                })();
                out.result(&id, result);
            }
            _ => {
                if busy
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_err()
                {
                    out.result(
                        &id,
                        Err(anyhow::anyhow!("agent busy; use steer, abort or approve")),
                    );
                    continue;
                }
                interrupt.store(false, Ordering::SeqCst);
                if commands.send(request).is_err() {
                    busy.store(false, Ordering::SeqCst);
                    break;
                }
            }
        }
    }
    interrupt.store(true, Ordering::SeqCst);
    drop(commands);
    drop(approval_tx);
    worker
        .join()
        .map_err(|_| anyhow::anyhow!("RPC worker panicked"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn oversized_request_is_drained_without_consuming_the_next_request() {
        let mut bytes = vec![b'x'; MAX_REQUEST + 100];
        bytes.extend_from_slice(b"\n{\"id\":1,\"method\":\"state\"}\n");
        let mut input = std::io::BufReader::with_capacity(32, std::io::Cursor::new(bytes));
        assert!(read_request(&mut input).unwrap().unwrap().is_err());
        let next = read_request(&mut input).unwrap().unwrap().unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&next).unwrap()["method"],
            "state"
        );
        assert!(read_request(&mut input).unwrap().is_none());
    }
    #[test]
    fn complete_request_without_final_newline_is_accepted() {
        let mut input = std::io::Cursor::new(b"{\"id\":2,\"method\":\"state\"}");
        let next = read_request(&mut input).unwrap().unwrap().unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&next).unwrap()["id"], 2);
    }
}
