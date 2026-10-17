//! Browser `run` and `call` actions: pure request parsing, validation and output shaping.
//! Execution lives in `browser_driver/run.cjs` (page-realm JavaScript plus the allowlisted
//! page-helper table). Nothing here touches Chromium, so every function is unit-testable.

use serde_json::{Value, json};
use std::borrow::Cow;

/// Largest accepted `code` or `fn` source, in bytes.
pub const RUN_SOURCE_MAX_BYTES: usize = 64 * 1024;
/// Largest accepted argument list for a `fn` run (and for each `call` step), by element count.
pub const RUN_ARGS_MAX_COUNT: usize = 32;
/// Largest accepted argument list for a `fn` run (and for each `call` step), by serialized bytes.
pub const RUN_ARGS_MAX_BYTES: usize = 64 * 1024;
/// Most steps one `call` chain may contain.
pub const RUN_CALL_MAX_STEPS: usize = 8;
/// Inline output budget (displayed text plus the stringified return value) returned to the model.
/// Protocol payloads stay bounded by `MAX_BODY` (4 MiB); this inline cap is deliberately far smaller.
pub const RUN_OUTPUT_MAX_BYTES: usize = 64 * 1024;
const _: () = assert!(RUN_OUTPUT_MAX_BYTES <= super::MAX_BODY);
/// Default run/call budget when `timeout` is omitted.
pub const RUN_TIMEOUT_DEFAULT_SECS: u64 = 30;
/// Lower clamp for `timeout` (seconds).
pub const RUN_TIMEOUT_MIN_SECS: u64 = 1;
/// Upper clamp for `timeout` (seconds); matches the browser tool's global 3600s ceiling.
pub const RUN_TIMEOUT_MAX_SECS: u64 = 3600;
/// Page-helper names accepted by `call`, in driver order. MUST match `HELPERS` in `browser_driver/run.cjs`.
pub const CALL_METHODS: &[&str] = &[
    "url",
    "title",
    "goto",
    "click",
    "fill",
    "type",
    "press",
    "check",
    "uncheck",
    "hover",
    "focus",
    "select",
    "waitForSelector",
    "evaluate",
    "screenshot",
];

/// Source of a `run` action.
#[derive(Clone, Debug, PartialEq)]
pub enum RunSource {
    /// Statements forming the body of an async function evaluated in the page (`code`).
    Code(String),
    /// A function expression source invoked with JSON `args` in the page (`fn`).
    Function { source: String, args: Vec<Value> },
}

/// A validated `run` request.
#[derive(Clone, Debug, PartialEq)]
pub struct RunRequest {
    pub source: RunSource,
    pub timeout_ms: u64,
}

/// One allowlisted page-helper invocation in a `call` chain.
#[derive(Clone, Debug, PartialEq)]
pub struct CallStep {
    pub method: String,
    pub args: Vec<Value>,
}

/// A validated `call` request. Steps run sequentially; the driver returns every step's result in order.
#[derive(Clone, Debug, PartialEq)]
pub struct CallRequest {
    pub chain: Vec<CallStep>,
    pub timeout_ms: u64,
}

impl RunRequest {
    /// Page-realm body passed to the driver's `runCode`. A `fn` source becomes an awaited immediate call.
    pub fn code(&self) -> Cow<'_, str> {
        match &self.source {
            RunSource::Code(code) => Cow::Borrowed(code.as_str()),
            RunSource::Function { source, args } => Cow::Owned(format!(
                "return await (\n{source}\n)(...{});",
                Value::Array(args.clone())
            )),
        }
    }

    /// Request object for the driver's `runCode` (the caller adds the `page` handle).
    pub fn to_driver_json(&self) -> Value {
        json!({ "code": self.code(), "timeoutMs": self.timeout_ms })
    }
}

impl CallRequest {
    /// Request object for the driver's `runCall` (the caller adds the `page` handle).
    pub fn to_driver_json(&self) -> Value {
        let chain: Vec<Value> = self
            .chain
            .iter()
            .map(|step| json!({ "method": step.method, "args": step.args }))
            .collect();
        json!({ "chain": chain, "timeoutMs": self.timeout_ms })
    }
}

/// Parse and validate the arguments of a `run` action.
///
/// Exactly one of `code` (function body) or `fn` (function source) is required. Empty strings count as absent.
/// `args` is valid only with `fn`. `timeout` is seconds, clamped to 1..=3600, default 30.
pub fn parse_run(args: &Value) -> Result<RunRequest, String> {
    let code = optional_str(args, "code")?;
    let function = optional_str(args, "fn")?;
    let source = match (code, function) {
        (Some(code), None) => {
            check_source("code", &code)?;
            if !parse_args(args)?.is_empty() {
                return Err("args is only valid with 'fn'".into());
            }
            RunSource::Code(code)
        }
        (None, Some(source)) => {
            check_source("fn", &source)?;
            let args = parse_args(args)?;
            RunSource::Function { source, args }
        }
        _ => return Err("run requires exactly one of 'code' or 'fn'".into()),
    };
    Ok(RunRequest {
        source,
        timeout_ms: parse_timeout_ms(args)?,
    })
}

/// Parse and validate the arguments of a `call` action.
///
/// `chain` is a non-empty array of at most [`RUN_CALL_MAX_STEPS`] `{method, args}` steps. Each `method` must be in
/// [`CALL_METHODS`]; unknown names are rejected here with the full allowlist in the message.
pub fn parse_call(args: &Value) -> Result<CallRequest, String> {
    let steps = match args.get("chain") {
        Some(Value::Array(steps)) if !steps.is_empty() => steps,
        Some(Value::Array(_)) | None | Some(Value::Null) => {
            return Err("call requires a non-empty 'chain' array".into());
        }
        Some(_) => return Err("chain must be an array".into()),
    };
    if steps.len() > RUN_CALL_MAX_STEPS {
        return Err(format!(
            "call chains allow at most {RUN_CALL_MAX_STEPS} steps"
        ));
    }
    let chain = steps
        .iter()
        .enumerate()
        .map(|(index, step)| parse_step(index, step))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(CallRequest {
        chain,
        timeout_ms: parse_timeout_ms(args)?,
    })
}

/// Shape a driver result into the tool result `{text, truncated, total_bytes, cap_bytes}`.
///
/// `driver` is the object returned by `runCode`/`runCall`: optional `displays` (array of strings), and either
/// `value` (any JSON) or `value_preview` + `truncated` + `total_chars` when the page-side encoder cut it.
/// `text` is the display lines followed by the pretty-printed value. When it exceeds `cap` bytes it is cut on a
/// char boundary and a marker states the cap and the full size.
pub fn shape_output(driver: &Value, cap: usize) -> Value {
    let mut parts: Vec<String> = driver
        .get("displays")
        .and_then(Value::as_array)
        .map(|displays| {
            displays
                .iter()
                .filter_map(Value::as_str)
                .map(|display| display.trim_end_matches('\n').to_string())
                .collect()
        })
        .unwrap_or_default();
    if let Some(preview) = driver.get("value_preview").and_then(Value::as_str) {
        parts.push(preview.to_string());
    } else if let Some(value) = driver.get("value") {
        parts.push(serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()));
    }

    let mut text = parts.join("\n");
    let total_bytes = text.len();
    let page_truncated = driver
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let over_cap = total_bytes > cap;
    if over_cap {
        text = truncate_at_char_boundary(&text, cap).to_string();
        text.push_str(&format!(
            "\n[output truncated: first {cap} bytes shown of {total_bytes} bytes; inline cap is {cap} bytes]"
        ));
    }
    if page_truncated {
        let total_chars = driver
            .get("total_chars")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        text.push_str(&format!(
            "\n[value truncated in page: preview of {total_chars} characters total]"
        ));
    }
    json!({
        "text": text,
        "truncated": over_cap || page_truncated,
        "total_bytes": total_bytes,
        "cap_bytes": cap,
    })
}

fn optional_str(args: &Value, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => {
            let trimmed = text.trim();
            Ok((!trimmed.is_empty()).then(|| trimmed.to_string()))
        }
        Some(_) => Err(format!("{key} must be a string")),
    }
}

fn check_source(label: &str, source: &str) -> Result<(), String> {
    if source.len() > RUN_SOURCE_MAX_BYTES {
        return Err(format!("{label} exceeds {RUN_SOURCE_MAX_BYTES} bytes"));
    }
    Ok(())
}

fn parse_args(args: &Value) -> Result<Vec<Value>, String> {
    match args.get("args") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(list)) => {
            check_args(list)?;
            Ok(list.clone())
        }
        Some(_) => Err("args must be an array".into()),
    }
}

fn check_args(list: &[Value]) -> Result<(), String> {
    if list.len() > RUN_ARGS_MAX_COUNT {
        return Err(format!(
            "at most {RUN_ARGS_MAX_COUNT} arguments are allowed"
        ));
    }
    let bytes = serde_json::to_string(list)
        .map_err(|error| error.to_string())?
        .len();
    if bytes > RUN_ARGS_MAX_BYTES {
        return Err(format!(
            "arguments exceed {RUN_ARGS_MAX_BYTES} serialized bytes"
        ));
    }
    Ok(())
}

fn parse_timeout_ms(args: &Value) -> Result<u64, String> {
    let secs = match args.get("timeout") {
        None | Some(Value::Null) => RUN_TIMEOUT_DEFAULT_SECS as f64,
        Some(value) => value
            .as_f64()
            .filter(|secs| secs.is_finite())
            .ok_or_else(|| "timeout must be a number of seconds".to_string())?,
    };
    let clamped = secs.clamp(RUN_TIMEOUT_MIN_SECS as f64, RUN_TIMEOUT_MAX_SECS as f64);
    Ok((clamped * 1000.0).round() as u64)
}

fn parse_step(index: usize, step: &Value) -> Result<CallStep, String> {
    let method = step
        .get("method")
        .and_then(Value::as_str)
        .filter(|method| !method.is_empty())
        .ok_or_else(|| format!("chain[{index}].method must be a non-empty string"))?;
    if !CALL_METHODS.contains(&method) {
        return Err(format!(
            "unknown call method \"{method}\"; allowed: {}",
            CALL_METHODS.join(", ")
        ));
    }
    let args = match step.get("args") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(args)) => args.clone(),
        Some(_) => return Err(format!("chain[{index}].args must be an array")),
    };
    check_args(&args).map_err(|error| format!("chain[{index}]: {error}"))?;
    Ok(CallStep {
        method: method.to_string(),
        args,
    })
}

fn truncate_at_char_boundary(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests;
