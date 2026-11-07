//! Synthetic tool protocol check. Never dispatches a registry tool.
use super::*;
use serde_json::json;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum Outcome {
    TextOnly,
    InvalidInvocation,
    StructuredInvocation,
    TextInvocation,
    StructuredRoundtrip,
    TextRoundtrip,
}

impl Outcome {
    pub(in crate::app) fn description(self) -> &'static str {
        match self {
            Self::TextOnly => "Text only · no synthetic tool invocation received",
            Self::InvalidInvocation => {
                "Invalid synthetic invocation · tool compatibility not verified"
            }
            Self::StructuredInvocation => {
                "Structured invocation received · result continuation not verified"
            }
            Self::TextInvocation => {
                "Text-parsed invocation received · result continuation not verified"
            }
            Self::StructuredRoundtrip => {
                "Structured tool roundtrip verified · result continuation received"
            }
            Self::TextRoundtrip => {
                "Text-parsed tool roundtrip verified · native structured invocation not verified"
            }
        }
    }
}

pub(super) fn test(config: &Config, cancel: Arc<AtomicBool>, budget: Duration) -> Result<Outcome> {
    ensure!(!cancel.load(Ordering::SeqCst), "verification cancelled");
    let mut config = config.clone();
    config.retry.max_attempts = 1;
    config.retry.rotate_accounts = false;
    config.retry.wait_for_quota_reset = false;
    config.retry.fallback_chains.clear();
    config.worker_timeout_secs = 20;
    let timed_out = Arc::new(AtomicBool::new(false));
    let result = std::thread::scope(|scope| {
        let (done, rx) = std::sync::mpsc::channel::<()>();
        let flag = cancel.clone();
        let deadline = timed_out.clone();
        scope.spawn(move || {
            if matches!(
                rx.recv_timeout(budget),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ) {
                deadline.store(true, Ordering::SeqCst);
                flag.store(true, Ordering::SeqCst);
            }
        });
        let result = (|| {
            let mut backend = make_backend(&config)?.without_steering();
            let mut profile = backend.model_profile().cloned().unwrap_or_else(|| {
                crate::llm::profiles::ModelProfile::minimal(
                    &config.selection().provider,
                    &config.model,
                )
            });
            profile.max_tokens = Some(
                profile
                    .max_tokens
                    .filter(|n| *n > 0)
                    .unwrap_or(128)
                    .min(128),
            );
            profile.inherit_output_limit = false;
            backend = backend.with_profile(profile);
            let mut request = crate::llm::Chat {
                messages: vec![
                    json!({"role":"user","content":"Invoke dial_diagnostic_echo exactly once with value dial-probe. After the tool result, reply with only its receipt. This is a synthetic protocol test; do not request any other tools."}),
                ],
                tools: vec![
                    json!({"type":"function","function":{"name":"dial_diagnostic_echo","description":"Side-effect-free synthetic diagnostic echo.","parameters":{"type":"object","additionalProperties":false,"properties":{"value":{"type":"string","enum":["dial-probe"]}},"required":["value"]}}}),
                ],
                temperature: 0.,
                max_tokens: 128,
                seed: None,
            };
            ensure!(!cancel.load(Ordering::SeqCst), "verification cancelled");
            let reply =
                backend.chat_with_control(None, &request, 20, None, &mut |_| {}, cancel.clone())?;
            ensure!(!cancel.load(Ordering::SeqCst), "verification cancelled");
            if reply.tool_calls.is_empty() {
                return Ok(Outcome::TextOnly);
            }
            let call = &reply.tool_calls[0];
            if reply.tool_calls.len() != 1
                || call.id.is_empty()
                || call.name != "dial_diagnostic_echo"
                || call.malformed_arguments
                || call.arguments != json!({"value":"dial-probe"})
            {
                return Ok(Outcome::InvalidInvocation);
            }
            let partial = if reply.text_calls_lifted {
                Outcome::TextInvocation
            } else {
                Outcome::StructuredInvocation
            };
            // The receipt is absent from the initial prompt: continuation must consume the result.
            let receipt = format!("dial-receipt-{:016x}", rand::random::<u64>());
            request.messages.push(crate::llm::assistant_message(&reply));
            request.messages.push(json!({"role":"tool","tool_call_id":call.id,"content":json!({"receipt":receipt}).to_string()}));
            match backend.chat_with_control(None, &request, 20, None, &mut |_| {}, cancel.clone()) {
                Ok(reply) if reply.tool_calls.is_empty() && reply.text.trim() == receipt => {
                    Ok(if partial == Outcome::TextInvocation {
                        Outcome::TextRoundtrip
                    } else {
                        Outcome::StructuredRoundtrip
                    })
                }
                _ => Ok(partial),
            }
        })();
        drop(done);
        result
    });
    ensure!(!timed_out.load(Ordering::SeqCst), "verification timed out");
    ensure!(!cancel.load(Ordering::SeqCst), "verification cancelled");
    result
}

#[cfg(test)]
mod tests;
