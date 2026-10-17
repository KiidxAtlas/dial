use super::*;

fn http(status: u16, text: &str) -> ErrorClass {
    classify_http(status, text, None, None)
}

#[test]
fn overflow_messages_report_the_measured_prompt_size() {
    for (text, tokens) in [
        (
            "prompt is too long: 213462 tokens > 200000 maximum",
            213_462,
        ),
        (
            "The input token count (1196265) exceeds the maximum number of tokens allowed (1048575).",
            1_196_265,
        ),
        (
            "This model's maximum context length is 8192 tokens. However, you requested 9000 tokens (8800 in the messages, 200 in the completion).",
            8_800,
        ),
    ] {
        let error = anyhow::Error::new(Classified::new(ErrorClass::ContextOverflow, text));
        assert_eq!(overflow_prompt_tokens(&error), Some(tokens), "{text}");
    }
}

#[test]
fn provider_overflow_messages_are_context_overflow() {
    for (status, text) in [
        (400, "prompt is too long: 213462 tokens > 200000 maximum"), // Anthropic
        (
            400,
            "The input token count (1196265) exceeds the maximum number of tokens allowed (1048575).",
        ), // Gemini
        (400, "Input is too long for requested model."),             // Bedrock
        (
            400,
            "Your input exceeds the context window of this model. Please adjust your input and try again.",
        ), // OpenAI Responses
        (
            400,
            "This model's maximum context length is 128000 tokens. However, your messages resulted in 130000 tokens.",
        ), // OpenAI/vLLM
        (
            400,
            "Please reduce the length of the messages or completion.",
        ), // Groq
        (
            400,
            "Prompt contains 140000 tokens, too large for model with 131072 maximum context length",
        ), // Mistral
        (
            400,
            "the request exceeds the available context size, try increasing it",
        ), // llama.cpp
        (500, "requested tokens (9000) exceed context window of 8192"), // llama.cpp variants
        (400, "context_length_exceeded"),
        (
            400,
            "The number of tokens to keep from the initial prompt is greater than the context length.",
        ), // LM Studio
        (
            400,
            "prompt (9000 tokens) + max tokens (2048) exceeds the context (8192)",
        ),
    ] {
        assert_eq!(http(status, text), ErrorClass::ContextOverflow, "{text}");
    }
    let anthropic = anyhow::Error::new(Classified::new(ErrorClass::Fatal, "x"));
    assert!(!is_context_overflow(&anthropic));
    assert!(is_context_overflow(&anyhow::anyhow!(
        "model server reported a stream error: prompt filled the context window"
    )));
}

#[test]
fn payload_rejection_is_distinguished_from_token_overflow() {
    assert_eq!(
        http(413, "request_too_large: Request exceeds the maximum size"),
        ErrorClass::Payload
    );
    assert_eq!(http(413, "<empty body>"), ErrorClass::Payload);
    assert_eq!(
        http(413, "request_too_large: prompt is too long: 300000 tokens"),
        ErrorClass::ContextOverflow
    );
    assert_eq!(
        http(400, "image exceeds the limit of 5242880"),
        ErrorClass::ContextOverflow
    );
    assert_eq!(
        http(413, "image exceeds the limit of 5242880 bytes"),
        ErrorClass::Payload
    );
}

#[test]
fn quota_exhaustion_differs_from_throttles() {
    assert_eq!(
        http(
            429,
            "You exceeded your current quota, please check your plan and billing details. insufficient_quota"
        ),
        ErrorClass::QuotaExhausted { reset_at: None }
    );
    assert_eq!(
        http(
            429,
            "The usage limit has been reached (type: usage_limit_reached)"
        ),
        ErrorClass::QuotaExhausted { reset_at: None }
    );
    assert_eq!(
        http(
            429,
            "Rate limit reached for gpt-4o on tokens per min (TPM): Limit 30000"
        ),
        ErrorClass::RateLimited { retry_after: None }
    );
    assert_eq!(
        http(429, "Error (type: rate_limit_error)"),
        ErrorClass::RateLimited { retry_after: None }
    );
    assert_eq!(
        http(
            429,
            "Too many concurrent requests; concurrency limit reached"
        ),
        ErrorClass::RateLimited { retry_after: None }
    );
    assert_eq!(
        http(
            429,
            "You exceeded your current quota, please check your plan and billing details. See https://help.aliyun.com/error-code#token-limit"
        ),
        ErrorClass::RateLimited { retry_after: None }
    );
    assert_eq!(
        http(402, "Insufficient Balance"),
        ErrorClass::QuotaExhausted { reset_at: None }
    );
    assert_eq!(
        http(
            403,
            "Your organization has reached its monthly spending limit"
        ),
        ErrorClass::QuotaExhausted { reset_at: None }
    );
    assert_eq!(
        http(529, "Overloaded (type: overloaded_error)"),
        ErrorClass::RateLimited { retry_after: None }
    );
    let hinted = Some(Duration::from_secs(1));
    assert_eq!(
        classify_http(529, "Overloaded", hinted, None),
        ErrorClass::RateLimited {
            retry_after: hinted
        }
    );
    // A TPM throttle phrased as "too many tokens" is retried, not treated as a full context.
    assert_eq!(
        http(
            429,
            "Rate limit exceeded: too many tokens per minute, limit of 100000"
        ),
        ErrorClass::RateLimited { retry_after: None }
    );
    assert_eq!(
        http(400, "image exceeds the limit of 5242880 bytes"),
        ErrorClass::ContextOverflow
    );
}

#[test]
fn google_retry_delay_separates_throttle_from_quota() {
    let delay = Some(Duration::from_secs(12));
    let throttle = r#"{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"Resource has been exhausted, please retry later.","details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"12s"}]}}"#;
    assert_eq!(
        classify_http(429, throttle, delay, None),
        ErrorClass::RateLimited { retry_after: delay }
    );
    let daily = throttle.replace("please retry later", "GenerateRequestsPerDay quota");
    assert_eq!(
        classify_http(429, &daily, delay, None),
        ErrorClass::QuotaExhausted { reset_at: None }
    );
    assert_eq!(
        classify_http(429, throttle, None, None),
        ErrorClass::QuotaExhausted { reset_at: None }
    );
}

#[test]
fn auth_transient_and_fatal_statuses() {
    assert_eq!(http(401, "invalid x-api-key"), ErrorClass::Auth);
    assert_eq!(http(403, "permission denied"), ErrorClass::Auth);
    assert_eq!(http(503, "unavailable"), ErrorClass::Transient);
    assert_eq!(
        http(500, "Failed to parse tool call arguments as JSON"),
        ErrorClass::Fatal
    );
    assert_eq!(
        http(
            400,
            "Floating point NaN (not-a-number) is detected in generation"
        ),
        ErrorClass::Transient
    );
    assert_eq!(http(400, "messages: unexpected role"), ErrorClass::Fatal);
    assert_eq!(http(404, "model not found"), ErrorClass::Fatal);
}

#[test]
fn text_only_stream_errors_classify() {
    assert_eq!(
        classify(&anyhow::anyhow!(
            "model server reported a stream error: Overloaded"
        )),
        ErrorClass::RateLimited { retry_after: None }
    );
    assert_eq!(
        classify(&anyhow::anyhow!(
            "model server reported a stream error: Internal server error"
        )),
        ErrorClass::Transient
    );
    assert_eq!(
        classify(&anyhow::anyhow!("chat response is not valid JSON")),
        ErrorClass::Fatal
    );
    assert_eq!(
        classify(&anyhow::Error::new(ThinkingLoop("cycle".into()))),
        ErrorClass::Transient
    );
    assert_eq!(
        classify(&anyhow::Error::new(super::super::Interrupted)),
        ErrorClass::Fatal
    );
}

#[test]
fn class_survives_context_and_redaction_wrappers() {
    let error = anyhow::Error::new(Classified::new(
        ErrorClass::ContextOverflow,
        "details withheld",
    ))
    .context("provider request failed");
    assert!(is_context_overflow(&error));
}

#[test]
fn failed_requests_are_labelled_by_class_without_provider_text() {
    let limited = anyhow::Error::new(Classified::new(
        ErrorClass::RateLimited { retry_after: None },
        "secret-token in provider text",
    ))
    .context("provider request failed");
    assert_eq!(classify(&limited).label(), "rate_limited");
    let other = anyhow::anyhow!("something unclassified");
    assert_eq!(classify(&other).label(), "fatal");
}
