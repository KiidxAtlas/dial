//! Drive one Dial turn without any terminal UI.
//!
//! ```sh
//! cargo run -p dial-client --example headless -- "summarize this repository"
//! ```
//!
//! Set `DIAL_BIN` to choose the engine binary (default: `dial` on `PATH`) and `DIAL_RPC_ARGS`
//! for extra space-separated flags after `rpc`, such as `--model local`. Approval requests are
//! denied: an unattended example must never grant permission on the user's behalf.

use dial_client::{Client, Decision, Launch, Message};
use serde_json::json;
use std::io::Write;
use std::time::Duration;

const STARTUP: Duration = Duration::from_secs(60);
const REPLY: Duration = Duration::from_secs(10);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let prompt = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    if prompt.trim().is_empty() {
        return Err("usage: headless <prompt>".into());
    }
    let program = std::env::var_os("DIAL_BIN").unwrap_or_else(|| "dial".into());
    let extra = std::env::var("DIAL_RPC_ARGS").unwrap_or_default();
    let launch = Launch::new(program).args(extra.split_whitespace());

    let mut client = Client::connect(&launch, STARTUP)?;
    eprintln!(
        "connected: model {} protocol {}",
        client.ready().model,
        client.protocol_version()
    );

    let turn = client.request("send", json!({ "text": prompt }))?;
    let mut stdout = std::io::stdout();
    loop {
        let Some(message) = client.poll(Duration::from_millis(250)) else {
            continue;
        };
        match message {
            Message::Event(event) => {
                if let Some(text) = event.text_delta() {
                    write!(stdout, "{text}")?;
                    stdout.flush()?;
                } else if let Some(approval) = event.approval() {
                    eprintln!("\n[denying {}: {}]", approval.tool, approval.summary);
                    client.approve(&approval.approval_id, Decision::Deny, REPLY)?;
                } else if event.kind == "tool_call" {
                    eprintln!("\n[tool {}]", event.body["name"].as_str().unwrap_or("?"));
                }
            }
            Message::Response { id, result, .. } if id == turn => {
                writeln!(stdout)?;
                match result {
                    Ok(reply) => eprintln!("[turn finished: {}]", reply["outcome"]),
                    Err(error) => eprintln!("[turn failed: {error}]"),
                }
                break;
            }
            Message::Response { .. } => {}
            Message::ProtocolError(error) => eprintln!("\n[protocol error: {error}]"),
            Message::Disconnected { outstanding } => {
                // Never retried automatically: a lost `send` may already have run tools.
                for request in outstanding {
                    eprintln!(
                        "\n[outcome unknown: {} ({:?})]",
                        request.method, request.effect
                    );
                }
                return Err("engine exited".into());
            }
        }
    }
    client.shutdown(REPLY)?;
    Ok(())
}
