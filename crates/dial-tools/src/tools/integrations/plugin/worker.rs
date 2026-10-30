//! Session-owned framed transport. Admission remains held until the entire child group is gone.
use super::{HostCall, Installation};
use crate::runtime::json_transport::{JsonProcess, execution_deadline};
use anyhow::{Result, anyhow, ensure};
use serde_json::{Value, json};
use std::{
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI32, Ordering},
    },
    time::Duration,
};

struct Session {
    process: JsonProcess,
    _lease: Option<super::lease::Lease>,
}
#[derive(Default)]
enum State {
    #[default]
    Fresh,
    Ready(Session),
    Failed,
    Closed,
}
#[derive(Default)]
pub(super) struct Worker {
    state: Arc<parking_lot::Mutex<State>>,
    closed: AtomicBool,
}
pub(super) struct Request<'a, 'host> {
    pub installation: Option<&'a Installation>,
    pub timeout: Duration,
    pub method: &'a str,
    pub params: Value,
    pub interrupt: &'a dyn Fn() -> bool,
    pub host: &'a mut HostCall<'host>,
    pub declaration: Option<&'a Value>,
}
impl Worker {
    pub(super) fn shutdown(&self) {
        self.closed.store(true, Ordering::Release);
        // An active call checks `closed`; do not wait here while holding the host registry lock.
        if let Some(mut state) = self.state.try_lock() {
            *state = State::Closed;
        } else {
            let state = Arc::clone(&self.state);
            std::thread::spawn(move || {
                *state.lock() = State::Closed;
            });
        }
    }
    pub(super) fn restart(&self) -> Result<()> {
        ensure!(
            !self.closed.load(Ordering::Acquire),
            "extension worker is closed"
        );
        let mut state = self
            .state
            .try_lock()
            .ok_or_else(|| anyhow!("extension worker is busy"))?;
        *state = State::Fresh;
        Ok(())
    }
    pub(super) fn request(
        &self,
        command: impl FnOnce() -> Command,
        request: Request<'_, '_>,
    ) -> Result<Value> {
        let Request {
            installation,
            timeout,
            method,
            params,
            interrupt,
            host,
            declaration,
        } = request;
        ensure!(
            !self.closed.load(Ordering::Acquire),
            "extension worker is closed"
        );
        let mut state = self
            .state
            .try_lock()
            .ok_or_else(|| anyhow!("extension worker is busy"))?;
        let check = || {
            if interrupt() || self.closed.load(Ordering::Acquire) {
                Err("extension request cancelled or owner closed".to_owned())
            } else {
                Ok(())
            }
        };
        if matches!(*state, State::Fresh) {
            let started = (|| -> Result<Session> {
                let lease = installation.map(Installation::admit).transpose()?;
                let process = JsonProcess::start(command(), Arc::new(AtomicI32::new(0)))
                    .map_err(anyhow::Error::msg)?;
                Ok(Session {
                    process,
                    _lease: lease,
                })
            })();
            match started {
                Ok(session) => *state = State::Ready(session),
                Err(error) => {
                    *state = State::Failed;
                    return Err(error);
                }
            }
            if let Some(expected) = declaration {
                let State::Ready(session) = &mut *state else {
                    unreachable!()
                };
                let initialized = exchange(
                    session,
                    timeout,
                    "initialize",
                    super::protocol::initialize_params(),
                    &check,
                    host,
                )
                .and_then(decode);
                match initialized {
                    Ok(value) if &value == expected => {}
                    Ok(_) => {
                        *state = State::Failed;
                        return Err(anyhow!(
                            "extension declarations changed; rebuild the extension registration"
                        ));
                    }
                    Err(error) => {
                        *state = State::Failed;
                        return Err(error);
                    }
                }
            }
        }
        let State::Ready(session) = &mut *state else {
            return Err(anyhow!(
                "extension worker is unavailable; explicitly restart it"
            ));
        };
        let result = exchange(session, timeout, method, params, &check, host);
        // Protocol errors returned by the extension are ordinary values, not transport failures.
        if result.is_err() || check().is_err() {
            *state = State::Failed;
        }
        let value = result?;
        let valid = value.get("response").is_some_and(|value| {
            value.is_object() && value.get("result").is_some() != value.get("error").is_some()
        });
        if !valid {
            *state = State::Failed;
            anyhow::bail!("invalid framed extension response");
        }
        decode(value)
    }
}
fn exchange(
    session: &mut Session,
    timeout: Duration,
    method: &str,
    params: Value,
    check: &impl Fn() -> std::result::Result<(), String>,
    host: &mut HostCall<'_>,
) -> Result<Value> {
    session
        .process
        .request_guarded(
            json!({"jsonrpc":"2.0", "method":method,"params":params}),
            execution_deadline(timeout),
            check,
            256,
            |frame| {
                let callback = &frame["callback"];
                let method = callback
                    .get("method")
                    .and_then(Value::as_str)
                    .ok_or("invalid extension callback method")?;
                let params = callback.get("params").unwrap_or(&Value::Null);
                host(method, params).map_err(|error| format!("{error:#}"))
            },
        )
        .map_err(anyhow::Error::msg)
}
fn decode(value: Value) -> Result<Value> {
    let response = value
        .get("response")
        .ok_or_else(|| anyhow!("missing framed extension response"))?;
    ensure!(
        response.is_object() && response.get("result").is_some() != response.get("error").is_some(),
        "invalid framed extension response"
    );
    if let Some(error) = response.get("error") {
        anyhow::bail!("extension JSON-RPC error: {error}");
    }
    Ok(response["result"].clone())
}

#[cfg(test)]
mod tests;
