//! Explicit, same-origin dashboard scale jobs. The normal weight owner validates and runs them.
use super::{
    Config, Context, Operation, Response, Result, TEXT, ensure, json_response, route, status,
};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};

type Job = Arc<parking_lot::Mutex<Value>>;

pub(super) fn respond(config: &Config, job: &Job, request: &str) -> Response {
    let Some((headers, body)) = request.split_once("\r\n\r\n") else {
        return bad("incomplete request");
    };
    let mut line = headers
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace();
    let method = line.next().unwrap_or_default();
    let target = line.next().unwrap_or_default();
    if method == "GET" {
        if target == "/api/status" {
            let mut value = status(config);
            value["dashboard_job"] = job.lock().clone();
            return json_response(Ok(value));
        }
        return route(config, target);
    }
    if method != "POST" || target != "/api/scale" {
        return (
            "405 Method Not Allowed",
            TEXT,
            "use GET or POST /api/scale".into(),
        );
    }
    match action_input(config, headers, body) {
        Ok((op, input)) => {
            let mut slot = job.lock();
            if slot["running"] == true {
                return (
                    "409 Conflict",
                    TEXT,
                    "a dashboard job is already running".into(),
                );
            }
            *slot = json!({"running": true, "operation": op.name()});
            let config = config.clone();
            let shared = Arc::clone(job);
            match std::thread::Builder::new().name("dial-weights-edit".into()).spawn(move || {
                let layout = crate::config::paths::Layout {
                    project_root: config.project_root.clone(),
                    project_dir: config.project_root.join(".dial"),
                    user_dir: config.user_dir.clone(),
                    state_dir: config.state_dir.clone(),
                };
                let result = super::super::run(&config, Some(&layout), op, &input, None, &AtomicBool::new(false));
                *shared.lock() = match result {
                    Ok(value) => json!({"running": false, "operation": op.name(), "result": value}),
                    Err(error) => json!({"running": false, "operation": op.name(), "error": format!("{error:#}")}),
                };
            }) {
                Ok(_) => ("202 Accepted", TEXT, "job started".into()),
                Err(error) => {
                    *slot = Value::Null;
                    ("500 Internal Server Error", TEXT, format!("starting weight job: {error}"))
                }
            }
        }
        Err(error) => bad(&format!("{error:#}")),
    }
}

fn bad(message: &str) -> Response {
    ("400 Bad Request", TEXT, message.into())
}

fn action_input(config: &Config, headers: &str, body: &str) -> Result<(Operation, Value)> {
    let header = |key: &str| {
        headers
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case(key))
            .map(|(_, value)| value.trim())
    };
    // A foreign page cannot send this non-simple header without a CORS preflight; no CORS
    // permissions are served. Also reject mismatched Origin even for non-browser clients.
    ensure!(
        header("x-dial-weights") == Some("1"),
        "missing dashboard action header"
    );
    if let Some(origin) = header("origin") {
        ensure!(
            Some(origin.strip_prefix("http://").unwrap_or_default()) == header("host"),
            "unexpected Origin"
        );
    }
    ensure!(
        header("content-type") == Some("application/json"),
        "expected application/json"
    );
    ensure!(
        headers.len() <= 8192 && body.len() <= 65536,
        "request too large"
    );
    ensure!(
        crate::stats::server::content_length(headers) == Some(body.len()),
        "invalid Content-Length"
    );
    let input: Value = serde_json::from_str(body).context("invalid scale request JSON")?;
    ensure!(
        input["model"].as_str() == Some(super::super::target_name(config)),
        "model changed; reload the dashboard"
    );
    let op = match input["action"].as_str() {
        Some("preview") => Operation::Try,
        Some("apply") => Operation::Edit,
        _ => anyhow::bail!("action must be preview or apply"),
    };
    let experiment = input["experiment"]
        .as_str()
        .context("choose an existing experiment")?;
    super::super::experiment(config, experiment)?;
    let mut value =
        json!({"method": "scale", "changes": input["changes"], "experiment": experiment});
    if op == Operation::Try {
        value["cases"] = input["cases"].clone();
        value["doses"] = json!([1.0]);
    } else {
        value["name"] = input["name"].clone();
    }
    super::super::arguments(op, &value)?;
    Ok((op, value))
}

#[cfg(test)]
mod tests;
