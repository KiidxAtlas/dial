//! Streaming model requests with UI heartbeats, stream guards and request recovery.

use crate::agent::*;

pub(in crate::agent) enum ChatEvent {
    Retry {
        waiting: bool,
    },
    Progress(llm::ModelProgress),
    Text(String),
    Reasoning(String),
    ToolArguments {
        index: u64,
        id: String,
        name: String,
        delta: String,
    },
    Notice(String),
    Finished(Box<Result<llm::Reply>>),
}

/// Run blocking work on a scoped thread while the UI keeps painting and taking input (interrupts,
/// steering). The agent itself stays on the UI thread; only the blocking call moves.
pub(in crate::agent) fn blocking_with_heartbeat<T: Send>(
    ui: &mut dyn Ui,
    work: impl FnOnce() -> T + Send,
) -> Result<T> {
    thread::scope(|scope| {
        let (tx, rx) = mpsc::sync_channel(1);
        scope.spawn(move || {
            let _ = tx.send(work());
        });
        receive_with_heartbeat(&rx, ui)
    })
}

pub(in crate::agent) fn receive_with_heartbeat<T>(rx: &Receiver<T>, ui: &mut dyn Ui) -> Result<T> {
    loop {
        match rx.recv_timeout(Duration::from_millis(80)) {
            Ok(event) => return Ok(event),
            Err(RecvTimeoutError::Timeout) => ui.heartbeat(),
            Err(RecvTimeoutError::Disconnected) => {
                bail!("background operation ended without a result")
            }
        }
    }
}

#[derive(Debug)]
pub(in crate::agent) struct SilentRequest {
    pub(in crate::agent) silence_secs: u64,
    pub(in crate::agent) direction: Option<String>,
    pub(in crate::agent) partial: String,
}
impl std::fmt::Display for SilentRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "The model request reported no progress for {}s; the task, conversation and completed tool/file work are kept.",
            self.silence_secs
        )
    }
}
impl std::error::Error for SilentRequest {}

/// Whether the generation-silence watchdog may cancel a request. Remote streams are always
/// watched. A local engine's model load and prompt prefill are long, silent, and bounded only by
/// the transport timeout, so only observed generation (text, reasoning, tool deltas, or a
/// `Generating` progress report) arms it. Any retry disarms it until generation resumes.
pub(in crate::agent) fn silence_watch_armed(local_model: bool, generating: bool) -> bool {
    !local_model || generating
}

/// Whether generation silence has lasted past its limit. Provider backoff is an explicit wait, not
/// a stall: it never arms the watchdog, and the clock restarts when the next attempt begins.
pub(in crate::agent) fn silence_exceeded(
    limit_secs: u64,
    local_model: bool,
    generating: bool,
    backing_off: bool,
    idle: Duration,
) -> bool {
    limit_secs > 0
        && !backing_off
        && silence_watch_armed(local_model, generating)
        && idle.as_secs() >= limit_secs
}

/// High-water marks: repeated status/rate reports are not evidence of inference progress.
#[derive(Default)]
struct TelemetryProgress {
    phase: u8,
    processed: u64,
    output: u64,
}

impl TelemetryProgress {
    fn observe(&mut self, value: &llm::ModelProgress) -> bool {
        let phase = match value.phase {
            llm::InferencePhase::Loading => 1,
            llm::InferencePhase::Processing => 2,
            llm::InferencePhase::Generating => 3,
        };
        let processed = value.processed_tokens.unwrap_or(0);
        let output = value.output_tokens.unwrap_or(0);
        let advanced = phase > self.phase || processed > self.processed || output > self.output;
        self.phase = self.phase.max(phase);
        self.processed = self.processed.max(processed);
        self.output = self.output.max(output);
        advanced
    }
}

/// Guarded attempts for a detected thinking/response loop, including the first.
pub(in crate::agent) const THINKING_LOOP_MAX_ATTEMPTS: u32 = 3;
pub(in crate::agent) use crate::agent::prompts::recovery::THINKING_LOOP_REDIRECT;

/// Per-call settings for `chat_with_ui`.
#[derive(Clone, Copy)]
pub(in crate::agent) struct ChatOptions<'a> {
    pub(in crate::agent) timeout: u64,
    pub(in crate::agent) render_text: bool,
    pub(in crate::agent) context_tokens: Option<u64>,
    pub(in crate::agent) policy: Option<&'a crate::config::types::ReasoningGuardConfig>,
    pub(in crate::agent) recovery: Option<&'a crate::config::types::RequestRecoveryConfig>,
}

pub(in crate::agent) fn chat_with_ui(
    backend: &mut Backend,
    adapter: Option<&Path>,
    request: &Chat,
    options: ChatOptions<'_>,
    interrupt: Arc<AtomicBool>,
    ui: &mut dyn Ui,
    on_idle: &mut dyn FnMut(&mut dyn Ui),
) -> Result<llm::Reply> {
    chat_with_ui_inner(
        backend, adapter, request, options, interrupt, ui, on_idle, None,
    )
}

pub(in crate::agent) fn chat_with_ui_inner(
    backend: &mut Backend,
    adapter: Option<&Path>,
    request: &Chat,
    options: ChatOptions<'_>,
    interrupt: Arc<AtomicBool>,
    ui: &mut dyn Ui,
    on_idle: &mut dyn FnMut(&mut dyn Ui),
    mut rules: Option<&mut crate::agent::safety::rules::Monitor<'_>>,
) -> Result<llm::Reply> {
    let mut automatically_retried = false;
    let mut loop_attempts = 1;
    let mut redirected: Option<Chat> = None;
    loop {
        let result = chat_attempt_with_ui(
            backend,
            adapter,
            redirected.as_ref().unwrap_or(request),
            options,
            interrupt.clone(),
            ui,
            on_idle,
            rules.as_deref_mut(),
        );
        let mut error = match result {
            Ok(reply) => return Ok(reply),
            Err(error)
                if error.downcast_ref::<llm::errors::ThinkingLoop>().is_some()
                    && !interrupt.load(Ordering::SeqCst)
                    && loop_attempts < THINKING_LOOP_MAX_ATTEMPTS =>
            {
                // Discard the looping attempt and re-sample the same model with a hidden,
                // request-local redirect. It is never journaled.
                loop_attempts += 1;
                // Local routes with `assistance.local.loop_penalty` re-sample with a presence and
                // repetition penalty (and thinking off where switchable) for this retry only.
                let penalty = if backend.retry_with_loop_penalty() {
                    " with a repetition penalty"
                } else {
                    ""
                };
                ui.inference_discarded(&format!(
                    "{error}; discarding the looping attempt and re-sampling{penalty} ({loop_attempts}/{THINKING_LOOP_MAX_ATTEMPTS})."
                ));
                let redirect = match error
                    .downcast_ref::<crate::agent::safety::stream_guards::GeminiHeaderRunaway>()
                {
                    Some(runaway) => {
                        crate::agent::safety::stream_guards::render_gemini_tool_call_reminder(
                            runaway.count,
                        )
                    }
                    None => THINKING_LOOP_REDIRECT.to_owned(),
                };
                let mut retry = request.clone();
                retry
                    .messages
                    .push(json!({"role":"user","content":redirect,"dial_transient":true}));
                redirected = Some(retry);
                continue;
            }
            Err(error)
                if error
                    .downcast_ref::<SilentRequest>()
                    .is_some_and(|stall| stall.partial.trim().is_empty())
                    && !interrupt.load(Ordering::SeqCst) =>
            {
                error
            }
            Err(error) => return Err(error),
        };
        if !automatically_retried && options.recovery.is_some_and(|policy| policy.retry_once) {
            automatically_retried = true;
            ui.diagnostic("Retrying only the silent inference request once, with the same context. Completed tools are not replayed.");
            continue;
        }
        if options.render_text && ui.interactive_input() {
            let choices = vec![
                "Retry only this request".into(),
                "Pause and keep completed work".into(),
            ];
            if let Some(answer) =
                ui.ask_user("Model request needs recovery", &error.to_string(), &choices)
            {
                if answer == choices[0] {
                    continue;
                }
                if !answer.trim().is_empty() && answer != choices[1] {
                    error.downcast_mut::<SilentRequest>().unwrap().direction = Some(answer);
                }
            }
        }
        return Err(error);
    }
}

pub(in crate::agent) fn chat_attempt_with_ui(
    backend: &mut Backend,
    adapter: Option<&Path>,
    request: &Chat,
    options: ChatOptions<'_>,
    interrupt: Arc<AtomicBool>,
    ui: &mut dyn Ui,
    on_idle: &mut dyn FnMut(&mut dyn Ui),
    mut rules: Option<&mut crate::agent::safety::rules::Monitor<'_>>,
) -> Result<llm::Reply> {
    if let Some(monitor) = rules.as_deref_mut() {
        monitor.restart();
    }
    let ChatOptions {
        timeout,
        render_text,
        context_tokens,
        policy,
        recovery,
    } = options;
    let external_interrupt = interrupt;
    let interrupt = Arc::new(AtomicBool::new(false));
    let cancel_request = interrupt.clone();
    let silence_limit = recovery.map_or(0, |policy| policy.silence_secs);
    let local_model = backend.local_inference();
    let mut context =
        context_tokens.map(crate::agent::window::context_meter::StreamingContext::new);
    if let Some(tokens) = context_tokens {
        ui.context_usage(tokens, true);
    }
    let (tx, rx) = mpsc::channel();
    let mut progress = backend
        .local_inference()
        .then(crate::agent::workflow::progress::RequestProgress::new);
    if let Some(progress) = &progress {
        ui.model_progress(&progress.value);
    }
    ui.request_phase(RequestPhase::Waiting);
    ui.model_wait(Duration::ZERO);
    // Model class picks the detectors; `None` means the kill switch is set.
    let loop_policy = llm::thinking_loop::policy_for(backend.model_label());
    thread::scope(|scope| {
        let worker = thread::Builder::new()
            .name("dial-model".to_owned())
            .spawn_scoped(scope, move || {
                let mut on_text = |text: &str| {
                    tx.send(ChatEvent::Text(text.to_owned())).is_ok()
                        && !interrupt.load(Ordering::SeqCst)
                };
                let mut on_notice = |notice: &str| {
                    let _ = tx.send(ChatEvent::Notice(notice.to_owned()));
                };
                let mut on_event = |event: &llm::StreamEvent| {
                    match event {
                        llm::StreamEvent::Retry { waiting } => {
                            let _ = tx.send(ChatEvent::Retry { waiting: *waiting });
                        }
                        llm::StreamEvent::Progress(value) => {
                            let _ = tx.send(ChatEvent::Progress(value.clone()));
                        }
                        llm::StreamEvent::Reasoning(text) => {
                            let _ = tx.send(ChatEvent::Reasoning(text.clone()));
                        }
                        llm::StreamEvent::ToolArguments {
                            index,
                            id,
                            name,
                            delta,
                        } => {
                            let _ = tx.send(ChatEvent::ToolArguments {
                                index: *index,
                                id: id.clone(),
                                name: name.clone(),
                                delta: delta.clone(),
                            });
                        }
                    }
                    !interrupt.load(Ordering::SeqCst)
                };
                let result = if let Some(policy) = policy {
                    crate::agent::workflow::reasoning::chat(
                        backend,
                        adapter,
                        request,
                        timeout,
                        Arc::clone(&interrupt),
                        policy,
                        crate::agent::workflow::reasoning::Sinks {
                            on_text: &mut on_text,
                            on_notice: &mut on_notice,
                            on_event: &mut on_event,
                        },
                    )
                } else {
                    backend.chat_with_control_events(
                        adapter,
                        request,
                        crate::llm::Limits {
                            timeout_secs: timeout,
                            interrupt: Arc::clone(&interrupt),
                        },
                        Some(&mut on_text),
                        &mut on_notice,
                        &mut on_event,
                    )
                };
                let _ = tx.send(ChatEvent::Finished(Box::new(result)));
            })
            .map_err(|error| anyhow::anyhow!("could not start model worker: {error}"))?;
        // Separate tails: reasoning and visible prose must not concatenate into one cycle. Visible
        // text latches the reasoning detector off.
        // Model class picks the detectors; `None` means the kill switch is set.
        let fresh_guard =
            || loop_policy.map(|p| llm::thinking_loop::ThinkingLoopGuard::new(p.semantic));
        let mut reasoning_loop = fresh_guard();
        let mut text_loop = fresh_guard();
        let mut header_loop = loop_policy
            .filter(|p| p.header)
            .map(|_| llm::thinking_loop::GeminiHeaderRunDetector::default());
        let mut header_runaway: Option<usize> = None;
        let mut thinking_loop_reason = None;
        let mut last_tick = Instant::now();
        let mut last_progress = Instant::now();
        let mut generating = false;
        let mut telemetry = TelemetryProgress::default();
        // True while the provider waits out a retry delay (`Retry { waiting: true }`).
        let mut backing_off = false;
        let mut stalled = None;
        let mut partial_text = String::new();
        let result = loop {
            if let Some(error) = ui.execution_error() {
                cancel_request.store(true, Ordering::SeqCst);
                break Err(anyhow::Error::msg(error));
            }
            if let Some(monitor) = rules
                .as_deref_mut()
                .filter(|m| m.hit.is_some() || m.failure.is_some())
            {
                cancel_request.store(true, Ordering::SeqCst);
                match rx.recv_timeout(Duration::from_millis(80)) {
                    Ok(ChatEvent::Finished(_)) | Err(RecvTimeoutError::Disconnected) => {
                        if let Some(failure) = &monitor.failure {
                            break Err(anyhow::anyhow!("{failure}"));
                        }
                        break Err(anyhow::Error::new(monitor.hit.clone().unwrap()));
                    }
                    _ => {
                        on_idle(ui);
                        continue;
                    }
                }
            }
            match rx.recv_timeout(Duration::from_millis(80)) {
                Ok(ChatEvent::Finished(_)) if stalled.is_some() => {
                    break Err(anyhow::Error::new(SilentRequest {
                        silence_secs: stalled.unwrap(),
                        direction: None,
                        partial: std::mem::take(&mut partial_text),
                    }));
                }
                Ok(_) if stalled.is_some() => {} // Never expose the cancelled attempt's late partial output.
                Ok(ChatEvent::Retry { waiting }) => {
                    ui.request_phase(if waiting {
                        RequestPhase::Waiting
                    } else {
                        RequestPhase::Retrying
                    });
                    last_progress = Instant::now();
                    generating = false;
                    telemetry = TelemetryProgress::default();
                    backing_off = waiting;
                    if waiting {
                        partial_text.clear();
                        // A discarded attempt's output must not count toward the next one. A loop
                        // already detected keeps its verdict; that attempt is being cancelled.
                        if thinking_loop_reason.is_none() {
                            reasoning_loop = fresh_guard();
                            text_loop = fresh_guard();
                            if let Some(header) = &mut header_loop {
                                header.reset();
                            }
                        }
                        if let Some(monitor) = rules.as_deref_mut() {
                            monitor.retry();
                        }
                        context = context_tokens
                            .map(crate::agent::window::context_meter::StreamingContext::new);
                        if let Some(tokens) = context_tokens {
                            ui.context_usage(tokens, true);
                        }
                        progress = progress
                            .as_ref()
                            .map(|_| crate::agent::workflow::progress::RequestProgress::new());
                    }
                }
                Ok(ChatEvent::Progress(value)) => {
                    ui.request_phase(match value.phase {
                        llm::InferencePhase::Loading => RequestPhase::Loading,
                        llm::InferencePhase::Processing => RequestPhase::Processing,
                        llm::InferencePhase::Generating => RequestPhase::Generating,
                    });
                    generating |= value.phase == llm::InferencePhase::Generating;
                    if telemetry.observe(&value) {
                        last_progress = Instant::now();
                    }
                    if let Some(context) = &mut context {
                        let (tokens, estimated) = context.telemetry(&value);
                        ui.context_usage(tokens, estimated);
                    }
                    if let Some(progress) = &mut progress {
                        progress.observe(value, last_progress);
                        ui.model_progress(&progress.value);
                    }
                }
                Ok(ChatEvent::Text(text) | ChatEvent::Reasoning(text)) if text.is_empty() => {}
                Ok(ChatEvent::Text(text)) => {
                    if rules.as_deref_mut().is_some_and(|m| !m.text(&text, false)) {
                        cancel_request.store(true, Ordering::SeqCst);
                        ui.inference_discarded("Portable guidance intercepted this inference (match or coverage limit); no attempted tools will execute.");
                        continue;
                    }
                    partial_text.push_str(&text);
                    if !text.is_empty() {
                        if let Some(mut guard) = reasoning_loop.take()
                            && thinking_loop_reason.is_none()
                        {
                            // Visible text ends the reasoning phase: flush its tail first.
                            thinking_loop_reason = guard.flush();
                            if thinking_loop_reason.is_some() {
                                cancel_request.store(true, Ordering::SeqCst);
                            }
                        }
                        if let Some(header) = &mut header_loop {
                            header.reset();
                        }
                    }
                    if thinking_loop_reason.is_none()
                        && let Some(guard) = &mut text_loop
                    {
                        thinking_loop_reason = guard.push(&text);
                        if thinking_loop_reason.is_some() {
                            cancel_request.store(true, Ordering::SeqCst);
                        }
                    }
                    if !text.is_empty() {
                        generating = true;
                        ui.request_phase(RequestPhase::Generating);
                    }
                    last_progress = Instant::now();
                    if let Some(progress) = &mut progress {
                        progress.text(&text, last_progress);
                        ui.model_progress(&progress.value);
                    }
                    if render_text {
                        if let Some(context) = &mut context {
                            ui.context_usage(context.observe(&text), context.estimated());
                        }
                        ui.text_delta(&text);
                    }
                }
                Ok(ChatEvent::Notice(notice)) => {
                    ui.notice(&notice);
                }
                Ok(ChatEvent::Reasoning(text)) => {
                    if rules.as_deref_mut().is_some_and(|m| !m.text(&text, true)) {
                        cancel_request.store(true, Ordering::SeqCst);
                        ui.inference_discarded("Portable guidance intercepted this inference (match or coverage limit); no attempted tools will execute.");
                        continue;
                    }
                    if thinking_loop_reason.is_none()
                        && let Some(guard) = &mut reasoning_loop
                    {
                        thinking_loop_reason = guard.push(&text);
                        if thinking_loop_reason.is_some() {
                            cancel_request.store(true, Ordering::SeqCst);
                        }
                    }
                    if thinking_loop_reason.is_none()
                        && let Some(header) = &mut header_loop
                        && let Some(count) = header.push(&text)
                    {
                        header_runaway = Some(count);
                        cancel_request.store(true, Ordering::SeqCst);
                    }
                    if !text.is_empty() {
                        generating = true;
                        ui.request_phase(RequestPhase::Generating);
                    }
                    last_progress = Instant::now();
                    if let Some(progress) = &mut progress {
                        progress.text(&text, last_progress);
                        ui.model_progress(&progress.value);
                    }
                    if render_text {
                        if let Some(context) = &mut context {
                            ui.context_usage(context.observe(&text), context.estimated());
                        }
                        ui.reasoning_delta(&text);
                    }
                }
                Ok(ChatEvent::ToolArguments {
                    index,
                    id,
                    name,
                    delta,
                }) => {
                    if rules
                        .as_deref_mut()
                        .is_some_and(|m| !m.tool(index.try_into().unwrap(), &name, &delta))
                    {
                        cancel_request.store(true, Ordering::SeqCst);
                        ui.inference_discarded("Portable tool guidance intercepted this inference (match or coverage limit); no partial tool call will execute.");
                        continue;
                    }
                    // A tool call starts the tool phase: the text detector stops watching.
                    text_loop = None;
                    if !name.is_empty()
                        && let Some(header) = &mut header_loop
                    {
                        header.reset();
                    }
                    if !delta.is_empty() || (!generating && !name.is_empty()) {
                        last_progress = Instant::now();
                        generating = true;
                        ui.request_phase(RequestPhase::Generating);
                    }
                    if let Some(progress) = &mut progress {
                        progress.text(&delta, last_progress);
                        ui.model_progress(&progress.value);
                    }
                    if render_text {
                        if let Some(context) = &mut context {
                            ui.context_usage(context.observe(&delta), context.estimated());
                        }
                        ui.tool_arguments_delta(index, &id, &name, &delta);
                    }
                }
                Ok(ChatEvent::Finished(result)) => {
                    let result = *result;
                    if let (Some(monitor), Ok(reply)) = (rules.as_deref_mut(), &result) {
                        if let Some(hit) = monitor.completed(reply) {
                            ui.inference_discarded("Portable guidance matched the completed response; no attempted tools will execute.");
                            break Err(anyhow::Error::new(hit));
                        }
                        if let Some(failure) = &monitor.failure {
                            ui.inference_discarded("Portable regex coverage limit reached; no attempted tools will execute.");
                            break Err(anyhow::anyhow!("{failure}"));
                        }
                    }
                    if let (Some(progress), Ok(reply)) = (&mut progress, &result)
                        && reply.completion_tokens.is_some()
                    {
                        let mut reported = llm::ModelProgress::new(llm::InferencePhase::Generating);
                        reported.output_tokens = reply.completion_tokens;
                        reported.prompt_tokens = reply.prompt_tokens;
                        progress.observe(reported, Instant::now());
                        ui.model_progress(&progress.value);
                    }
                    if let (Some(context), Ok(reply)) = (&context, &result) {
                        let (tokens, estimated) =
                            context.finish(reply.prompt_tokens, reply.completion_tokens);
                        ui.context_usage(tokens, estimated);
                    }
                    if thinking_loop_reason.is_none() && result.is_ok() {
                        thinking_loop_reason = [reasoning_loop.take(), text_loop.take()]
                            .into_iter()
                            .flatten()
                            .find_map(|mut guard| guard.flush());
                    }
                    break result;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    break Err(anyhow::anyhow!("model worker ended without a result"));
                }
            }
            // Pump input even when a role's hidden text continuously fills the event queue.
            if last_tick.elapsed() >= Duration::from_millis(80) {
                if stalled.is_none() {
                    ui.model_wait(last_progress.elapsed());
                }
                ui.heartbeat();
                on_idle(ui);
                if external_interrupt.load(Ordering::SeqCst) {
                    ui.request_phase(RequestPhase::Cancelling);
                    cancel_request.store(true, Ordering::SeqCst);
                } else if stalled.is_none()
                    && silence_exceeded(
                        silence_limit,
                        local_model,
                        generating,
                        backing_off,
                        last_progress.elapsed(),
                    )
                {
                    stalled = Some(last_progress.elapsed().as_secs());
                    ui.request_phase(RequestPhase::Cancelling);
                    cancel_request.store(true, Ordering::SeqCst);
                    ui.inference_discarded("Silent inference detected; cancelling only this request. The task and completed work are preserved.");
                }
                last_tick = Instant::now();
            }
        };
        if worker.join().is_err() {
            return Err(anyhow::anyhow!("model worker panicked"));
        }
        if external_interrupt.load(Ordering::SeqCst) {
            return Err(llm::Interrupted.into());
        }
        if let Some(count) = header_runaway {
            return Err(anyhow::Error::new(llm::errors::ThinkingLoop(format!(
                "{count} consecutive reasoning headers without a tool call"
            )))
            .context(crate::agent::safety::stream_guards::GeminiHeaderRunaway { count }));
        }
        if let Some(reason) = thinking_loop_reason {
            return Err(anyhow::Error::new(llm::errors::ThinkingLoop(reason)));
        }
        result
    })
}

#[cfg(test)]
mod silence_tests;
