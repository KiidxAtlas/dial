//! Provider sign-in over RPC (protocol 2): `login`, `auth_code`, `auth_cancel`, `logout` and
//! `auth_providers`. The flows and grant storage are the same owners `/login` uses.
//!
//! `login` runs the owner flow on a worker thread and replies to the request when it finishes.
//! While it runs, the flow emits `auth_url` (open this URL, optionally with a device code),
//! `auth_progress`, and `auth_prompt` (the host answers with `auth_code`). Typed answers are never
//! emitted or logged, and events carry no tokens. The reply carries only the owner's status line.

use super::Sender;
use crate::llm::oauth::{self, Interaction};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

const PROMPT_TIMEOUT: Duration = Duration::from_secs(600);
const POLL: Duration = Duration::from_millis(50);

/// Minimum negotiated protocol for the sign-in methods.
const MIN_PROTOCOL: u32 = 2;

struct Job {
    provider: &'static str,
    cancel: Arc<AtomicBool>,
}

struct Prompt {
    id: String,
    answer: mpsc::Sender<String>,
}

/// The single sign-in running at a time, and the prompt it is waiting on.
#[derive(Default)]
pub(super) struct AuthState {
    job: Mutex<Option<Job>>,
    prompt: Mutex<Option<Prompt>>,
    next_prompt: AtomicU64,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl AuthState {
    /// Registers a prompt, announces its id through `emit`, and waits for `answer`. `emit`
    /// receives only the prompt id, never the answer. The wait ends on cancel, on a gone client,
    /// or after [`PROMPT_TIMEOUT`].
    pub(super) fn ask(
        &self,
        emit: impl FnOnce(&str) -> bool,
        cancel: &AtomicBool,
        gone: impl Fn() -> bool,
    ) -> Result<String> {
        let id = format!("p{}", self.next_prompt.fetch_add(1, Ordering::SeqCst) + 1);
        let (answer, receiver) = mpsc::channel();
        *locked(&self.prompt) = Some(Prompt {
            id: id.clone(),
            answer,
        });
        let result = if emit(&id) {
            wait_for_answer(&receiver, cancel, gone)
        } else {
            Err(anyhow!(
                "no client is attached to answer the sign-in prompt"
            ))
        };
        locked(&self.prompt).take();
        result
    }

    /// Delivers the host's answer to the waiting prompt `id`.
    fn answer(&self, id: &str, code: String) -> Result<()> {
        let slot = locked(&self.prompt);
        let Some(prompt) = slot.as_ref().filter(|prompt| prompt.id == id) else {
            bail!("no sign-in prompt is waiting with that prompt_id");
        };
        prompt
            .answer
            .send(code)
            .map_err(|_| anyhow!("the sign-in prompt is no longer waiting"))
    }
}

fn wait_for_answer(
    receiver: &mpsc::Receiver<String>,
    cancel: &AtomicBool,
    gone: impl Fn() -> bool,
) -> Result<String> {
    let deadline = Instant::now() + PROMPT_TIMEOUT;
    loop {
        match receiver.recv_timeout(POLL) {
            Ok(answer) => return Ok(answer),
            Err(RecvTimeoutError::Disconnected) => bail!("sign-in was cancelled"),
            Err(RecvTimeoutError::Timeout) => {}
        }
        if cancel.load(Ordering::SeqCst) || gone() {
            bail!("sign-in was cancelled");
        }
        if Instant::now() >= deadline {
            bail!(
                "sign-in prompt timed out after {} seconds",
                PROMPT_TIMEOUT.as_secs()
            );
        }
    }
}

/// The owner-facing interaction. It forwards flow events to the host without typed secrets.
struct RpcInteraction<E, G> {
    provider: &'static str,
    state: Arc<AuthState>,
    /// Sends one frame to the host; `false` when the frame could not be delivered.
    emit: E,
    /// `true` once the host connection is gone.
    gone: G,
    cancel: Arc<AtomicBool>,
}

impl<E, G> Interaction for RpcInteraction<E, G>
where
    E: Fn(Value) -> bool + Send + Sync,
    G: Fn() -> bool + Send + Sync,
{
    fn auth(&self, url: &str, instructions: Option<&str>) {
        (self.emit)(json!({
            "event":"auth_url",
            "provider": self.provider,
            "url": url,
            "instructions": instructions,
        }));
    }

    fn progress(&self, message: &str) {
        (self.emit)(json!({
            "event":"auth_progress",
            "provider": self.provider,
            "message": message,
        }));
    }

    fn prompt(
        &self,
        message: &str,
        placeholder: Option<&str>,
        allow_empty: bool,
    ) -> Result<String> {
        self.state.ask(
            |prompt_id| {
                (self.emit)(json!({
                    "event":"auth_prompt",
                    "provider": self.provider,
                    "prompt_id": prompt_id,
                    "message": message,
                    "placeholder": placeholder,
                    "allow_empty": allow_empty,
                }))
            },
            &self.cancel,
            || (self.gone)(),
        )
    }
}

/// Rejects the sign-in methods on ACP and on connections that negotiated protocol 1.
pub(super) fn ensure_v2(out: &Sender) -> Result<()> {
    ensure!(
        out.mapping().is_ok_and(|output| output.acp.is_none()),
        "sign-in methods are not available over ACP"
    );
    ensure!(
        out.protocol() >= MIN_PROTOCOL,
        "sign-in methods need protocol_version {MIN_PROTOCOL}; send hello with that version first"
    );
    Ok(())
}

/// Starts `login`. The request is answered by the worker thread when the flow finishes.
pub(super) fn start_login(
    state: &Arc<AuthState>,
    out: &Sender,
    user_dir: &Path,
    id: &Value,
    params: &Value,
) -> Result<()> {
    let provider = params["provider"].as_str().context("provider required")?;
    let flow = oauth::flow(provider)
        .with_context(|| format!("unknown provider {provider:?}; call auth_providers"))?;
    ensure!(
        !flow.needs_terminal,
        "{} sign-in needs a terminal; run `dial login {}`",
        flow.name,
        flow.id
    );
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut job = locked(&state.job);
        ensure!(job.is_none(), "an authentication action is already running");
        *job = Some(Job {
            provider: flow.id,
            cancel: cancel.clone(),
        });
    }
    let state = state.clone();
    let out = out.clone();
    let user_dir = user_dir.to_path_buf();
    let id = id.clone();
    std::thread::spawn(move || {
        let emit_out = out.clone();
        let gone_out = out.clone();
        let ui = RpcInteraction {
            provider: flow.id,
            state: state.clone(),
            emit: move |frame| emit_out.emit(frame),
            gone: move || gone_out.unavailable(),
            cancel: cancel.clone(),
        };
        let result = oauth::login(&user_dir, flow.id, &ui, cancel)
            .map(|message| json!({"provider": flow.id, "message": message}));
        *locked(&state.job) = None;
        out.result(&id, result);
    });
    Ok(())
}

/// `auth_code`: the host's answer to an `auth_prompt`. The answer is not echoed back.
pub(super) fn submit_code(state: &AuthState, params: &Value) -> Result<Value> {
    let prompt_id = params["prompt_id"].as_str().context("prompt_id required")?;
    let code = params["code"].as_str().context("code must be a string")?;
    state.answer(prompt_id, code.to_owned())?;
    Ok(json!({"received": true}))
}

/// `auth_cancel`: aborts the running sign-in for `provider`.
pub(super) fn cancel_login(state: &AuthState, params: &Value) -> Result<Value> {
    let provider = params["provider"].as_str().context("provider required")?;
    let job = locked(&state.job);
    let Some(job) = job.as_ref().filter(|job| job.provider == provider) else {
        bail!("no sign-in for {provider} is running");
    };
    job.cancel.store(true, Ordering::SeqCst);
    Ok(json!({"provider": provider, "cancelling": true}))
}

/// `logout`: removes the stored grant. A sign-in running for the same provider is cancelled.
pub(super) fn logout(state: &AuthState, user_dir: &Path, params: &Value) -> Result<Value> {
    let provider = params["provider"].as_str().context("provider required")?;
    let flow = oauth::flow(provider)
        .with_context(|| format!("unknown provider {provider:?}; call auth_providers"))?;
    if let Some(job) = locked(&state.job)
        .as_ref()
        .filter(|job| job.provider == flow.id)
    {
        job.cancel.store(true, Ordering::SeqCst);
    }
    let message = oauth::sign_out(user_dir, flow.id)?;
    Ok(json!({"provider": flow.id, "message": message}))
}

/// `auth_providers`: sign-in providers with their signed-in state.
pub(super) fn providers(user_dir: &Path) -> Value {
    let providers: Vec<Value> = oauth::flows()
        .map(|flow| {
            json!({
                "provider": flow.id,
                "name": flow.name,
                "signed_in": oauth::signed_in(user_dir, flow.id),
                "needs_terminal": flow.needs_terminal,
            })
        })
        .collect();
    json!({ "providers": providers })
}

#[cfg(test)]
mod tests;
