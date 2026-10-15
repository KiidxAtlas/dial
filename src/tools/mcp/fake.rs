//! A scripted MCP server for the tests. The test binary re-executes itself (see `tests::fake_server`)
//! and this loop runs in place of the test harness, speaking the protocol on stdin/stdout.
//!
//! `flags` (comma separated, from the environment) switch off normal behavior:
//! `bad-protocol` answers `initialize` with an unknown version, `silent` never answers it,
//! `no-tools` lists nothing, `bad-list` answers `tools/list` with an error.

use serde_json::{Value, json};
use std::io::{BufRead, Write};

/// Environment variable that turns the test binary into this server; its value is the flag list.
pub const ENV: &str = "DIAL_FAKE_MCP_SERVER";

pub fn run(flags: &str) -> ! {
    let has = |flag: &str| flags.split(',').any(|f| f == flag);
    let mut ping_answered = false;
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let (id, method) = (
            message.get("id").cloned(),
            message.get("method").and_then(Value::as_str).unwrap_or(""),
        );
        // A response to our own `ping` (we only send one, with a string id).
        if method.is_empty()
            && message.get("id") == Some(&json!("srv-ping"))
            && message.get("result") == Some(&json!({}))
        {
            ping_answered = true;
            continue;
        }
        let Some(id) = id else { continue }; // notifications need no answer
        match method {
            "initialize" => {
                if has("silent") {
                    continue;
                }
                // Noise a real server may produce before answering.
                println!("starting up... (not JSON)");
                send(
                    &json!({"jsonrpc": "2.0", "method": "notifications/message", "params": {"level": "info", "data": "hi"}}),
                );
                send(&json!({"jsonrpc": "2.0", "id": "srv-ping", "method": "ping"}));
                let version = if has("bad-protocol") {
                    "1999-01-01"
                } else {
                    "2025-03-26"
                };
                send(&json!({"jsonrpc": "2.0", "id": id, "result": {
                    "protocolVersion": version,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "fake", "version": "0"},
                }}));
            }
            "tools/list" => send(&list(
                &id,
                message.pointer("/params/cursor").and_then(Value::as_str),
                flags,
            )),
            "tools/call" => {
                let name = message
                    .pointer("/params/name")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let args = message
                    .pointer("/params/arguments")
                    .cloned()
                    .unwrap_or(Value::Null);
                if let Some(result) = call(name, &args, ping_answered) {
                    send(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
                }
            }
            _ => send(
                &json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "no such method"}}),
            ),
        }
    }
    std::process::exit(0)
}

fn send(message: &Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{message}");
    let _ = out.flush();
}

fn tool(name: &str, read_only: Option<bool>) -> Value {
    let mut tool = json!({
        "name": name,
        "description": format!("fake tool {name}"),
        "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}},
    });
    if let Some(read_only) = read_only {
        tool["annotations"] = json!({"readOnlyHint": read_only});
    }
    tool
}

fn list(id: &Value, cursor: Option<&str>, flags: &str) -> Value {
    if flags.split(',').any(|f| f == "bad-list") {
        return json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": "list exploded"}});
    }
    if flags.split(',').any(|f| f == "no-tools") {
        return json!({"jsonrpc": "2.0", "id": id, "result": {"tools": []}});
    }
    let (tools, next) = match cursor {
        None => (
            vec![
                tool("echo", Some(true)),
                tool("plain", None),
                tool("fail", Some(false)),
            ],
            Some("page-2"),
        ),
        Some("page-2") => (
            vec![
                tool("mixed", None),
                tool("slow", None),
                tool("die", None),
                tool("pid", Some(true)),
            ],
            Some("page-3"),
        ),
        _ => {
            // No inputSchema at all, and names that collide once sanitised.
            let bare = json!({"name": "bare"});
            (
                vec![
                    tool("a b", None),
                    tool("a.b", None),
                    tool("big", None),
                    tool("pong", None),
                    bare,
                ],
                None,
            )
        }
    };
    let mut result = json!({"tools": tools});
    if let Some(next) = next {
        result["nextCursor"] = json!(next);
    }
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn text(text: impl Into<String>) -> Value {
    json!({"content": [{"type": "text", "text": text.into()}]})
}

/// `None` means "never answer" (a hung server).
fn call(name: &str, args: &Value, ping_answered: bool) -> Option<Value> {
    Some(match name {
        "echo" => text(format!(
            "echo: {}",
            args.get("text").and_then(Value::as_str).unwrap_or("")
        )),
        "plain" => text("plain ok"),
        "fail" => json!({"isError": true, "content": [{"type": "text", "text": "it broke"}]}),
        "mixed" => json!({"content": [
            {"type": "text", "text": "caption"},
            {"type": "image", "data": "AAAA", "mimeType": "image/png"},
            {"type": "resource", "resource": {"uri": "file:///r.txt", "text": "resource body"}},
        ]}),
        "big" => text("x".repeat(200 * 1024)),
        "pong" => text(format!("ping answered: {ping_answered}")),
        "pid" => text(std::process::id().to_string()),
        "bare" => text(format!("bare args: {args}")),
        "slow" => return None,
        "die" => {
            eprintln!("fatal: kaboom");
            std::process::exit(3)
        }
        _ => {
            json!({"isError": true, "content": [{"type": "text", "text": format!("unknown tool {name}")}]})
        }
    })
}
