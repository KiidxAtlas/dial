#![cfg(test)]
//! End-to-end tests of the client and the tool against an in-process fake language server
//! talking over a unix socket pair: real framing, real threads, no external binaries.

use crate::config::tools::lsp::LspConfig;

use super::client::{Client, ServerOptions, Timeouts, UnsettledRequest};
use super::framing::{read_message, write_message};
use super::*;
use crate::config::tools::lsp::LspServerConfig;
use crate::config::types::Config;
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

type Log = Arc<Mutex<Vec<Value>>>;

fn fast() -> Timeouts {
    Timeouts {
        initialize: Duration::from_secs(5),
        request: Duration::from_secs(5),
        shutdown: Duration::from_millis(500),
        diagnostics: Duration::from_secs(3),
        settle: Duration::from_millis(50),
    }
}

fn reply(out: &mut UnixStream, id: &Value, result: Value) {
    let _ = write_message(out, &json!({"jsonrpc": "2.0", "id": id, "result": result}));
}

fn publish(out: &mut UnixStream, uri: &Value, text: &str) {
    let diagnostics: Vec<Value> = text
        .lines()
        .enumerate()
        .filter(|(_, line)| line.contains("ERROR"))
        .map(|(n, _)| {
            json!({"range": {"start": {"line": n, "character": 0}, "end": {"line": n, "character": 5}},
                   "severity": 1, "source": "fake", "message": "found ERROR"})
        })
        .collect();
    let _ = write_message(
        out,
        &json!({
            "jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
            "params": {"uri": uri, "diagnostics": diagnostics},
        }),
    );
}

/// A language server that echoes positions, so tests can see exactly what the client sent.
fn serve(stream: UnixStream, log: Log) {
    serve_with_rename(stream, log, || Value::Null);
}

fn serve_with_rename(stream: UnixStream, log: Log, mut rename: impl FnMut() -> Value) {
    let mut out = stream.try_clone().unwrap();
    let mut input = BufReader::new(stream);
    while let Ok(Some(message)) = read_message(&mut input) {
        log.lock().push(message.clone());
        let params = &message["params"];
        let uri = &params["textDocument"]["uri"];
        let position = params["position"].clone();
        let here = json!({"uri": uri, "range": {"start": position, "end": position}});
        let (Some(method), id) = (message["method"].as_str(), message.get("id").cloned()) else {
            continue;
        };
        match (method, id) {
            ("workspace/willRenameFiles", Some(id)) => reply(&mut out, &id, rename()),
            ("initialize", Some(id)) => reply(
                &mut out,
                &id,
                json!({"capabilities": {"documentFormattingProvider": true,
                    "workspace": {"fileOperations": {"willRename": {"filters": [{"pattern": {"glob": "**/*"}}]}}}}}),
            ),
            ("initialized", None) => {
                let _ = write_message(
                    &mut out,
                    &json!({"jsonrpc": "2.0", "id": "cfg", "method": "workspace/configuration", "params": {"items": [{}, {}]}}),
                );
                let _ = write_message(
                    &mut out,
                    &json!({"jsonrpc": "2.0", "id": 99, "method": "window/workDoneProgress/create", "params": {"token": "t"}}),
                );
            }
            ("textDocument/didOpen", None) => publish(
                &mut out,
                &params["textDocument"]["uri"],
                params["textDocument"]["text"].as_str().unwrap(),
            ),
            ("textDocument/didChange", None) => publish(
                &mut out,
                uri,
                params["contentChanges"][0]["text"].as_str().unwrap(),
            ),
            (
                "textDocument/definition"
                | "textDocument/implementation"
                | "textDocument/typeDefinition",
                Some(id),
            ) => reply(&mut out, &id, here),
            ("textDocument/references", Some(id)) => {
                reply(&mut out, &id, json!([here.clone(), here]))
            }
            ("textDocument/hover", Some(id)) => reply(
                &mut out,
                &id,
                json!({"contents": {"kind": "markdown", "value": "hover!"}}),
            ),
            ("textDocument/documentSymbol", Some(id)) => reply(
                &mut out,
                &id,
                json!([{"name": "thing", "kind": 12, "location": {"uri": uri, "range": here["range"]}}]),
            ),
            ("workspace/symbol", Some(id)) => {
                let name = format!("sym-{}", params["query"].as_str().unwrap());
                reply(
                    &mut out,
                    &id,
                    json!([{"name": name, "kind": 12, "location": {"uri": "file:///elsewhere/x.fk"}}]),
                )
            }
            ("textDocument/rename", Some(id)) => {
                let edit = json!({"range": here["range"], "newText": params["newName"]});
                reply(
                    &mut out,
                    &id,
                    json!({"changes": {uri.as_str().unwrap(): [edit]}}),
                )
            }
            ("textDocument/formatting", Some(id)) => {
                // Every pass inserts a line, so a formatter re-run on its own commit never converges.
                reply(
                    &mut out,
                    &id,
                    json!([{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "newText": "// formatted\n"}]),
                )
            }
            ("test/fail", Some(id)) => {
                let _ = write_message(
                    &mut out,
                    &json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": "boom"}}),
                );
            }
            ("test/never", Some(_)) => {}
            ("test/hangup", Some(_)) => return,
            ("shutdown", Some(id)) => reply(&mut out, &id, Value::Null),
            ("exit", None) => return,
            (_, Some(id)) => {
                let _ = write_message(
                    &mut out,
                    &json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "nope"}}),
                );
            }
            _ => {}
        }
    }
}

fn fake_client(root: &Path, timeouts: Timeouts) -> (Client, Log) {
    let (ours, theirs) = UnixStream::pair().unwrap();
    let log = Log::default();
    let server_log = Arc::clone(&log);
    thread::spawn(move || serve(theirs, server_log));
    let input = ours.try_clone().unwrap();
    let client = Client::connect(
        "fake",
        root,
        Box::new(input),
        Box::new(ours),
        None,
        timeouts,
        ServerOptions::default(),
    )
    .unwrap();
    (client, log)
}

fn methods(log: &Log) -> Vec<String> {
    log.lock()
        .iter()
        .filter_map(|m| m["method"].as_str().map(str::to_owned))
        .collect()
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn project() -> (tempfile::TempDir, Workspace) {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    (dir, ws)
}

#[test]
fn handshake_announces_the_project_and_answers_server_requests() {
    let (_dir, ws) = project();
    let (_client, log) = fake_client(ws.root(), fast());
    let init = log.lock()[0].clone();
    assert_eq!(init["method"], "initialize");
    assert_eq!(init["params"]["rootUri"], uri_from_path(ws.root()));
    wait_until("replies to the server's requests", || {
        let log = log.lock();
        let answer = |id: Value| {
            log.iter()
                .find(|m| m["id"] == id && m.get("method").is_none())
                .cloned()
        };
        answer(json!("cfg")).is_some_and(|m| m["result"] == json!([null, null]))
            && answer(json!(99)).is_some_and(|m| m["result"].is_null() && m.get("error").is_none())
    });
    assert!(methods(&log).contains(&"initialized".to_string()));
}

#[test]
fn concurrent_requests_receive_their_own_responses() {
    let (_dir, ws) = project();
    let (client, _log) = fake_client(ws.root(), fast());
    thread::scope(|scope| {
        for n in 0..16 {
            let client = &client;
            scope.spawn(move || {
                let result = client
                    .request("workspace/symbol", json!({"query": n.to_string()}))
                    .unwrap();
                assert_eq!(result[0]["name"], format!("sym-{n}"));
            });
        }
    });
}

#[test]
fn server_errors_timeouts_and_exits_become_errors() {
    let (_dir, ws) = project();
    let timeouts = Timeouts {
        request: Duration::from_millis(200),
        ..fast()
    };
    let (client, log) = fake_client(ws.root(), timeouts);

    let error = client
        .request("test/fail", Value::Null)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("-32000") && error.contains("boom"),
        "{error}"
    );

    let error = client.request("test/never", Value::Null).unwrap_err();
    assert!(error.is::<UnsettledRequest>(), "{error:#}");
    let error = format!("{error:#}");
    assert!(error.contains("did not answer test/never"), "{error}");
    wait_until("cancel notification", || {
        methods(&log).contains(&"$/cancelRequest".to_string())
    });

    let error = client.request("test/hangup", Value::Null).unwrap_err();
    assert!(error.is::<UnsettledRequest>(), "{error:#}");
    let error = format!("{error:#}");
    assert!(error.contains("exited"), "{error}");
    wait_until("client to notice the exit", || !client.is_alive());
    assert!(
        client
            .request("textDocument/hover", Value::Null)
            .unwrap_err()
            .to_string()
            .contains("exited")
    );
}

#[test]
fn documents_are_opened_once_changed_on_edit_and_closed_when_deleted() {
    let (dir, ws) = project();
    let (client, log) = fake_client(ws.root(), fast());
    let (a, b) = (ws.root().join("a.fk"), ws.root().join("b.fk"));

    assert!(client.sync_document(&a, "fake", "one").unwrap());
    assert!(
        !client.sync_document(&a, "fake", "one").unwrap(),
        "unchanged content sends nothing"
    );
    assert!(client.sync_document(&a, "fake", "two").unwrap());
    std::fs::write(&b, "bee").unwrap();
    assert!(client.sync_document(&b, "fake", "bee").unwrap());

    std::fs::write(&b, "bee, edited").unwrap();
    std::fs::remove_file(&a).ok();
    client.refresh_open_documents(&dir.path().join("elsewhere"));
    wait_until("didClose and didChange", || {
        let m = methods(&log);
        m.contains(&"textDocument/didClose".to_string())
            && m.iter().filter(|m| *m == "textDocument/didChange").count() == 2
    });

    let log = log.lock();
    let change_to_b = log
        .iter()
        .rev()
        .find(|m| m["method"] == "textDocument/didChange")
        .unwrap();
    assert_eq!(change_to_b["params"]["textDocument"]["version"], 2);
    assert_eq!(
        change_to_b["params"]["contentChanges"][0]["text"],
        "bee, edited"
    );
    let open = log
        .iter()
        .find(|m| m["method"] == "textDocument/didOpen")
        .unwrap();
    assert_eq!(open["params"]["textDocument"]["languageId"], "fake");
    assert_eq!(open["params"]["textDocument"]["version"], 1);
}

#[test]
fn diagnostics_wait_for_a_publish_after_the_change_and_none_means_timeout() {
    let (_dir, ws) = project();
    let timeouts = Timeouts {
        diagnostics: Duration::from_millis(300),
        ..fast()
    };
    let (client, _log) = fake_client(ws.root(), timeouts);
    let path = ws.root().join("a.fk");

    let mark = client.diagnostics_mark(&path);
    let fresh = client
        .sync_document(&path, "fake", "ok\nERROR here")
        .unwrap();
    let items = client.diagnostics(&path, mark, fresh).expect("published");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["range"]["start"]["line"], 1);

    // Fixing the file must yield the new (empty) list, not the stale one.
    let mark = client.diagnostics_mark(&path);
    let fresh = client.sync_document(&path, "fake", "ok").unwrap();
    assert_eq!(client.diagnostics(&path, mark, fresh), Some(vec![]));

    // Unchanged document: the last publish is returned immediately.
    let mark = client.diagnostics_mark(&path);
    assert_eq!(client.diagnostics(&path, mark, false), Some(vec![]));

    // A document the server never published for: nothing arrives in time.
    assert_eq!(
        client.diagnostics(&ws.root().join("never.fk"), 0, true),
        None
    );
}

#[test]
fn dropping_the_client_runs_shutdown_then_exit() {
    let (_dir, ws) = project();
    let (client, log) = fake_client(ws.root(), fast());
    drop(client);
    wait_until("exit notification", || {
        methods(&log).last().is_some_and(|m| m == "exit")
    });
    let m = methods(&log);
    assert_eq!(&m[m.len() - 2..], ["shutdown", "exit"]);
}

// ---- the tool ----

struct Harness {
    _dir: tempfile::TempDir,
    ws: Workspace,
    tool: LspTool,
    starts: Arc<AtomicUsize>,
    logs: Arc<Mutex<Vec<Log>>>,
}

impl Harness {
    fn new() -> Self {
        Self::with_launch_error(None)
    }

    fn with_launch_error(error: Option<&'static str>) -> Self {
        let (dir, ws) = project();
        let config = LspConfig {
            servers: BTreeMap::from([(
                "fake".to_string(),
                LspServerConfig {
                    program: PathBuf::from("/bin/sh"),
                    args: Some(vec![]),
                    extensions: Some(vec!["fk".into()]),
                    ..LspServerConfig::default()
                },
            )]),
            ..LspConfig::default()
        };
        let (starts, logs) = (
            Arc::new(AtomicUsize::new(0)),
            Arc::new(Mutex::new(Vec::new())),
        );
        let (counter, collected) = (Arc::clone(&starts), Arc::clone(&logs));
        let tool = LspTool::with_launcher(
            &config,
            Box::new(move |_server, root, _cancel| {
                counter.fetch_add(1, Ordering::SeqCst);
                if let Some(error) = error {
                    anyhow::bail!(error);
                }
                let (client, log) = fake_client(root, fast());
                collected.lock().push(log);
                Ok(client)
            }),
        );
        Self {
            _dir: dir,
            ws,
            tool,
            starts,
            logs,
        }
    }

    fn write(&self, rel: &str, content: &str) {
        std::fs::write(self.ws.root().join(rel), content).unwrap();
    }

    fn call(&self, args: Value) -> ToolOutput {
        let config = Config::default();
        let original = crate::llm::ToolCall {
            id: "fixture".into(),
            name: "lsp".into(),
            arguments: args,
            malformed_arguments: false,
        };
        let authorize = crate::tools::testkit::authorize_in_workspace(&self.ws, "lsp");
        self.tool.call(
            &ToolContext {
                workspace: &self.ws,
                config: &config,
                mutation_authorizer: Some(&authorize),
                confirm: None,
            },
            &original.arguments,
        )
    }

    fn ok(&self, args: Value) -> String {
        let out = self.call(args);
        assert!(!out.is_error, "{}", out.text);
        out.text
    }

    fn err(&self, args: Value) -> String {
        let out = self.call(args);
        assert!(out.is_error, "expected an error, got {}", out.text);
        out.text
    }

    fn log(&self) -> Log {
        Arc::clone(&self.logs.lock()[0])
    }
}

#[test]
fn position_actions_send_utf16_columns_and_print_character_columns() {
    let h = Harness::new();
    h.write("a.fk", "😀 foo()\nlet x = 1\n");
    // `foo` is the third character but starts at UTF-16 offset 3.
    let by_symbol =
        h.ok(json!({"action": "definition", "path": "a.fk", "line": 1, "symbol": "foo"}));
    assert_eq!(by_symbol, "1 definition\na.fk:1:3  😀 foo()");
    let by_column = h.ok(json!({"action": "definition", "path": "a.fk", "line": 1, "column": 3}));
    assert_eq!(by_column, by_symbol);

    let sent = h
        .log()
        .lock()
        .iter()
        .rev()
        .find(|m| m["method"] == "textDocument/definition")
        .cloned()
        .unwrap();
    assert_eq!(
        sent["params"]["position"],
        json!({"line": 0, "character": 3})
    );
    assert_eq!(
        sent["params"]["textDocument"]["uri"],
        uri_from_path(&h.ws.root().join("a.fk"))
    );

    assert_eq!(
        h.ok(json!({"action": "references", "path": "a.fk", "line": 2, "symbol": "x"}))
            .lines()
            .next(),
        Some("1 reference"),
        "identical locations are de-duplicated"
    );
    assert!(
        h.ok(json!({"action": "type_definition", "path": "a.fk", "line": 2, "column": 1}))
            .starts_with("1 type definition")
    );
    assert!(
        h.ok(json!({"action": "implementation", "path": "a.fk", "line": 2, "column": 1}))
            .starts_with("1 implementation")
    );
    assert_eq!(
        h.ok(json!({"action": "hover", "path": "a.fk", "line": 1, "column": 1})),
        "hover!"
    );
}

#[test]
fn ordered_file_resources_share_transaction_and_refuse_overwriting_external_recovery() {
    let h = Harness::new();
    h.write("before.fk", "original\n");
    let (client, _) = fake_client(h.ws.root(), fast());
    let from = h.ws.root().join("before.fk");
    let to = h.ws.root().join("after.fk");
    let snapshot = super::workspace_edit::Snapshot::capture(&h.ws).unwrap();
    let edit = json!({"documentChanges":[
        {"kind":"rename","oldUri":uri_from_path(&from),"newUri":uri_from_path(&to)},
        {"textDocument":{"uri":uri_from_path(&to),"version":null},"edits":[
            {"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":8}},"newText":"rewritten"}
        ]}
    ]});
    let plan = super::workspace_edit::Plan::prepare(&h.ws, &client, &snapshot, &edit).unwrap();
    plan.commit(&h.ws, &snapshot).unwrap();
    assert!(!from.exists());
    assert_eq!(std::fs::read_to_string(&to).unwrap(), "rewritten\n");
    std::fs::write(&to, "external owner\n").unwrap();
    assert!(plan.revert(&h.ws).is_err());
    assert!(!from.exists());
    assert_eq!(std::fs::read_to_string(&to).unwrap(), "external owner\n");
}

#[test]
fn resource_create_edit_delete_batches_validate_every_target_before_publication() {
    let h = Harness::new();
    let (client, _) = fake_client(h.ws.root(), fast());
    let created = h.ws.root().join("new.fk");
    let snapshot = super::workspace_edit::Snapshot::capture(&h.ws).unwrap();
    let edit = json!({"documentChanges":[
        {"kind":"create","uri":uri_from_path(&created)},
        {"textDocument":{"uri":uri_from_path(&created),"version":null},"edits":[
            {"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":0}},"newText":"new source\n"}
        ]}
    ]});
    let plan = super::workspace_edit::Plan::prepare(&h.ws, &client, &snapshot, &edit).unwrap();
    plan.commit(&h.ws, &snapshot).unwrap();
    assert_eq!(std::fs::read_to_string(&created).unwrap(), "new source\n");
    let snapshot = super::workspace_edit::Snapshot::capture(&h.ws).unwrap();
    let delete = json!({"documentChanges":[{"kind":"delete","uri":uri_from_path(&created)}]});
    super::workspace_edit::Plan::prepare(&h.ws, &client, &snapshot, &delete)
        .unwrap()
        .commit(&h.ws, &snapshot)
        .unwrap();
    assert!(!created.exists());
    let escape = json!({"documentChanges":[
        {"kind":"create","uri":uri_from_path(&created)},
        {"kind":"create","uri":uri_from_path(&h.ws.root().parent().unwrap().join("escape.fk"))}
    ]});
    assert!(super::workspace_edit::Plan::prepare(&h.ws, &client, &snapshot, &escape).is_err());
    assert!(!created.exists());
}

#[test]
fn symbol_and_diagnostic_actions_format_server_results() {
    let h = Harness::new();
    h.write("a.fk", "fn thing() {}\nERROR\n");
    assert_eq!(
        h.ok(json!({"action": "document_symbols", "path": "a.fk"}))
            .lines()
            .last(),
        Some("1:1  function thing")
    );
    let found = h.ok(json!({"action": "workspace_symbols", "path": "a.fk", "query": "q"}));
    assert_eq!(found, "1 symbols\n/elsewhere/x.fk  function sym-q");
    let diagnostics = h.ok(json!({"action": "diagnostics", "path": "a.fk"}));
    assert_eq!(
        diagnostics,
        "1 diagnostics in a.fk (1 errors, 0 warnings)\na.fk:2:1  error[fake] found ERROR"
    );
}

#[test]
fn diagnostics_follow_edits_made_on_disk_between_calls() {
    let h = Harness::new();
    h.write("a.fk", "ERROR\n");
    assert!(
        h.ok(json!({"action": "diagnostics", "path": "a.fk"}))
            .starts_with("1 diagnostics")
    );
    h.write("a.fk", "fixed\n");
    assert_eq!(
        h.ok(json!({"action": "diagnostics", "path": "a.fk"})),
        "no diagnostics for a.fk"
    );
    let m = methods(&h.log());
    assert_eq!(m.iter().filter(|m| *m == "textDocument/didOpen").count(), 1);
    assert_eq!(
        m.iter().filter(|m| *m == "textDocument/didChange").count(),
        1
    );
}

#[test]
fn rename_preview_lists_edits_and_leaves_files_untouched() {
    let h = Harness::new();
    h.write("a.fk", "let old = 1\n");
    let text = h.ok(json!({"action": "rename_preview", "path": "a.fk", "line": 1, "symbol": "old", "new_name": "fresh"}));
    assert!(
        text.starts_with("rename preview, NOT applied: 1 edits in 1 files"),
        "{text}"
    );
    assert!(
        text.contains("a.fk:1:5-1:5 -> \"fresh\"  | let old = 1"),
        "{text}"
    );
    assert_eq!(
        std::fs::read_to_string(h.ws.root().join("a.fk")).unwrap(),
        "let old = 1\n"
    );
}

#[test]
fn approval_is_needed_only_to_start_a_server() {
    let h = Harness::new();
    h.write("a.fk", "x\n");
    let args = json!({"action": "hover", "path": "a.fk", "line": 1, "column": 1});
    assert!(
        h.tool.needs_approval(&args),
        "first call would start the server"
    );
    h.ok(args.clone());
    assert!(
        !h.tool.needs_approval(&args),
        "running server needs no approval"
    );
    // Calls that can never start a server need none either.
    assert!(
        !h.tool
            .needs_approval(&json!({"action": "hover", "path": "a.unknownext"}))
    );
    assert!(!h.tool.needs_approval(&json!({"action": "hover"})));
}

#[test]
fn servers_start_lazily_once_and_restart_after_dying() {
    let h = Harness::new();
    h.write("a.fk", "x\n");
    h.write("b.fk", "y\n");
    assert_eq!(
        h.starts.load(Ordering::SeqCst),
        0,
        "nothing starts before the first call"
    );
    h.ok(json!({"action": "hover", "path": "a.fk", "line": 1, "column": 1}));
    h.ok(json!({"action": "hover", "path": "b.fk", "line": 1, "column": 1}));
    assert_eq!(
        h.starts.load(Ordering::SeqCst),
        1,
        "one server per language, shared by files"
    );

    let client = Arc::clone(h.tool.running.lock().unwrap().get("fake").unwrap());
    assert!(client.request("test/hangup", Value::Null).is_err());
    wait_until("server death", || !client.is_alive());
    drop(client);
    let args = json!({"action": "hover", "path": "a.fk", "line": 1, "column": 1});
    assert!(
        h.tool.needs_approval(&args),
        "a dead server must be approved again"
    );
    assert_eq!(h.ok(args), "hover!");
    assert_eq!(h.starts.load(Ordering::SeqCst), 2);
}

#[test]
fn bad_requests_fail_clearly_without_starting_a_server() {
    let h = Harness::new();
    h.write("a.fk", "one line\n");
    std::fs::create_dir(h.ws.root().join("dir")).unwrap();
    let cases = [
        (json!({"action": "fly", "path": "a.fk"}), "unknown action"),
        (json!({"path": "a.fk"}), "action"),
        (json!({"action": "hover"}), "path"),
        (
            json!({"action": "hover", "path": "a.fk", "column": 1}),
            "line",
        ),
        (
            json!({"action": "hover", "path": "a.fk", "line": 1}),
            "column",
        ),
        (
            json!({"action": "hover", "path": "a.fk", "line": 9, "column": 1}),
            "out of range",
        ),
        (
            json!({"action": "hover", "path": "a.fk", "line": 1, "symbol": "zzz"}),
            "not found on line 1",
        ),
        (
            json!({"action": "hover", "path": "../outside.fk", "line": 1, "column": 1}),
            "escapes the workspace",
        ),
        (
            json!({"action": "hover", "path": "/etc/hosts", "line": 1, "column": 1}),
            "outside the workspace",
        ),
        (
            json!({"action": "hover", "path": "dir", "line": 1, "column": 1}),
            "not a file",
        ),
        (
            json!({"action": "hover", "path": "missing.fk", "line": 1, "column": 1}),
            "not a file",
        ),
        (
            json!({"action": "workspace_symbols", "path": "a.fk"}),
            "query",
        ),
        (
            json!({"action": "rename_preview", "path": "a.fk", "line": 1, "column": 1}),
            "new_name",
        ),
    ];
    for (args, expected) in cases {
        let error = h.err(args.clone());
        assert!(error.contains(expected), "{args}: {error}");
    }
    h.write("a.unknownext", "x");
    assert!(
        h.err(json!({"action": "hover", "path": "a.unknownext", "line": 1, "column": 1}))
            .contains("no language server")
    );
    assert_eq!(h.starts.load(Ordering::SeqCst), 0);
}

#[test]
fn a_server_that_fails_to_start_is_reported_and_retried() {
    let h = Harness::with_launch_error(Some("exec format error"));
    h.write("a.fk", "x\n");
    let args = json!({"action": "hover", "path": "a.fk", "line": 1, "column": 1});
    let error = h.err(args.clone());
    assert!(
        error.contains("could not start the fake language server")
            && error.contains("exec format error"),
        "{error}"
    );
    h.err(args);
    assert_eq!(
        h.starts.load(Ordering::SeqCst),
        2,
        "failures are not cached"
    );
}

#[test]
fn spawning_a_missing_program_is_an_error_not_a_panic() {
    let (_dir, ws) = project();
    let error = Client::spawn(
        "ghost",
        Path::new("/definitely/not/here"),
        &[],
        ws.root(),
        fast(),
        ServerOptions::default(),
    )
    .err()
    .unwrap();
    assert!(
        error.to_string().contains("/definitely/not/here"),
        "{error}"
    );
}

#[test]
fn schema_lists_every_action_and_requires_action() {
    let tool = LspTool::new(&LspConfig::default());
    let schema = tool.schema();
    let function = &schema["function"];
    assert_eq!(function["name"], "lsp");
    assert_eq!(function["parameters"]["required"], json!(["action"]));
    let actions = function["parameters"]["properties"]["action"]["enum"]
        .as_array()
        .unwrap();
    assert_eq!(actions.len(), ACTIONS.len());
    assert!(
        function["description"]
            .as_str()
            .unwrap()
            .contains("approval")
    );
}

#[test]
fn rename_pairs_reject_missing_identical_and_existing_destinations() {
    let (dir, ws) = project();
    let root = dir.path();
    let error = rename_pairs(&ws, &root.join("missing.fk"), &root.join("x.fk")).unwrap_err();
    assert!(error.starts_with("source path does not exist"), "{error}");
    std::fs::write(root.join("a.fk"), "a").unwrap();
    std::fs::write(root.join("b.fk"), "b").unwrap();
    let a = root.join("a.fk");
    assert_eq!(
        rename_pairs(&ws, &a, &a).unwrap_err(),
        "source and destination paths are identical"
    );
    assert!(
        rename_pairs(&ws, &a, &root.join("b.fk"))
            .unwrap_err()
            .starts_with("destination already exists")
    );
    assert_eq!(
        rename_pairs(&ws, &a, &root.join("c.fk")).unwrap(),
        vec![(a.clone(), root.join("c.fk"))]
    );
}

#[test]
fn rename_directories_expand_to_files_and_skip_git() {
    let (dir, ws) = project();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src/inner")).unwrap();
    std::fs::create_dir_all(root.join("src/.git")).unwrap();
    std::fs::write(root.join("src/a.fk"), "a").unwrap();
    std::fs::write(root.join("src/inner/b.fk"), "b").unwrap();
    std::fs::write(root.join("src/.git/config"), "c").unwrap();
    let mut pairs = rename_pairs(&ws, &root.join("src"), &root.join("lib")).unwrap();
    pairs.sort();
    assert_eq!(
        pairs,
        vec![
            (root.join("src/a.fk"), root.join("lib/a.fk")),
            (root.join("src/inner/b.fk"), root.join("lib/inner/b.fk")),
        ]
    );
    assert!(
        rename_pairs(&ws, &root.join("src"), &root.join("src/nested"))
            .unwrap_err()
            .starts_with("destination is inside")
    );
}

/// Real framing with a deterministic mutation while willRenameFiles is outstanding.
fn rename_tool(hook: impl Fn() -> Value + Send + Sync + 'static) -> LspTool {
    let hook = Arc::new(hook);
    LspTool::with_launcher(
        &fake_config(false),
        Box::new(move |_, root, _| {
            let (ours, theirs) = UnixStream::pair().unwrap();
            let hook = Arc::clone(&hook);
            thread::spawn(move || serve_with_rename(theirs, Log::default(), || hook()));
            Client::connect(
                "fake",
                root,
                Box::new(ours.try_clone().unwrap()),
                Box::new(ours),
                None,
                fast(),
                ServerOptions::default(),
            )
        }),
    )
}

fn reference_edit(path: &Path) -> Value {
    json!({"changes": {text::uri_from_path(path): [{
        "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}},
        "newText": "new"
    }]}})
}

#[test]
fn rename_file_refuses_sources_changed_during_will_rename_even_without_text_edits() {
    for edits in [false, true] {
        let (_dir, ws) = project();
        let source = ws.root().join("a.fk");
        let reference = ws.root().join("ref.fk");
        std::fs::write(&source, "old").unwrap();
        std::fs::write(&reference, "old").unwrap();
        let changed = source.clone();
        let edit = reference_edit(&reference);
        let tool = rename_tool(move || {
            std::fs::write(&changed, "external change").unwrap();
            if edits { edit.clone() } else { Value::Null }
        });
        let out = run_as(
            &tool,
            &ws,
            json!({"action": "rename_file", "path": "a.fk", "new_name": "b.fk"}),
        );
        assert!(out.is_error, "{}", out.text);
        assert!(
            out.text.contains("changed since the server request"),
            "{}",
            out.text
        );
        assert_eq!(std::fs::read_to_string(&source).unwrap(), "external change");
        assert_eq!(std::fs::read_to_string(&reference).unwrap(), "old");
        assert!(!ws.root().join("b.fk").exists());
    }
}

#[test]
fn rename_file_refuses_cross_file_edits_changed_during_will_rename() {
    let (_dir, ws) = project();
    let source = ws.root().join("a.fk");
    let reference = ws.root().join("ref.fk");
    std::fs::write(&source, "source").unwrap();
    std::fs::write(&reference, "old").unwrap();
    let changed = reference.clone();
    let edit = reference_edit(&reference);
    let tool = rename_tool(move || {
        std::fs::write(&changed, "external reference change").unwrap();
        edit.clone()
    });
    let out = run_as(
        &tool,
        &ws,
        json!({"action": "rename_file", "path": "a.fk", "new_name": "b.fk"}),
    );
    assert!(out.is_error, "{}", out.text);
    assert!(
        out.text.contains("changed since the server request"),
        "{}",
        out.text
    );
    assert_eq!(std::fs::read_to_string(source).unwrap(), "source");
    assert_eq!(
        std::fs::read_to_string(reference).unwrap(),
        "external reference change"
    );
    assert!(!ws.root().join("b.fk").exists());
}

struct DestinationAppears(PathBuf);

impl crate::tools::WriteObserver for DestinationAppears {
    fn before_write(&self, path: &Path) -> anyhow::Result<()> {
        if path == self.0 {
            std::fs::write(path, "external destination")?;
        }
        Ok(())
    }
}

#[test]
fn rename_file_destination_appearing_at_install_preserves_both_and_reverts_edits() {
    let (_dir, ws) = project();
    let source = ws.root().join("a.fk");
    let target = ws.root().join("b.fk");
    let reference = ws.root().join("ref.fk");
    std::fs::write(&source, "source").unwrap();
    std::fs::write(&reference, "old").unwrap();
    let edit = reference_edit(&reference);
    let tool = rename_tool(move || edit.clone());
    let ws = ws.with_observer(Arc::new(DestinationAppears(target.clone())));
    let out = run_as(
        &tool,
        &ws,
        json!({"action": "rename_file", "path": "a.fk", "new_name": "b.fk"}),
    );
    assert!(out.is_error, "{}", out.text);
    assert!(
        out.text.contains("text edits were restored"),
        "{}",
        out.text
    );
    assert_eq!(std::fs::read_to_string(source).unwrap(), "source");
    assert_eq!(
        std::fs::read_to_string(target).unwrap(),
        "external destination"
    );
    assert_eq!(std::fs::read_to_string(reference).unwrap(), "old");
}

#[test]
fn rename_moves_reverse_completed_units_without_replacing_external_destinations() {
    let (_dir, ws) = project();
    let a = ws.root().join("a");
    let b = ws.root().join("b");
    let c = ws.root().join("c");
    let d = ws.root().join("d");
    std::fs::write(&a, "a").unwrap();
    std::fs::write(&c, "c").unwrap();
    std::fs::write(&d, "external").unwrap();
    let error = move_units(&[(a.clone(), b.clone()), (c.clone(), d.clone())]).unwrap_err();
    assert!(error.contains("moved files were restored"), "{error}");
    assert_eq!(std::fs::read_to_string(a).unwrap(), "a");
    assert!(!b.exists());
    assert_eq!(std::fs::read_to_string(c).unwrap(), "c");
    assert_eq!(std::fs::read_to_string(d).unwrap(), "external");
}

#[test]
fn merged_workspace_edits_keep_every_server_edit_in_one_document_list() {
    assert_eq!(merge_workspace_edits(&[Value::Null]).unwrap(), Value::Null);
    let merged = merge_workspace_edits(&[
        json!({"changes": {"file:///a.fk": [{"newText": "x"}]}}),
        json!({"documentChanges": [{"textDocument": {"uri": "file:///b.fk", "version": 3}, "edits": []}]}),
    ])
    .unwrap();
    let entries = merged["documentChanges"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["textDocument"]["uri"], "file:///a.fk");
    assert_eq!(entries[1]["textDocument"]["version"], 3);
    assert!(merged.get("changes").is_none());
    assert!(merge_workspace_edits(&[json!({"changes": []})]).is_err());
}

// ---- write-through ownership and format-on-write ----

type Launcher = Box<
    dyn Fn(&super::servers::Resolved, &Path, &dyn Fn() -> bool) -> anyhow::Result<Client>
        + Send
        + Sync,
>;

/// Starts every server with a fake transport and keeps each server's transcript, in start order.
fn recording_launcher(logs: &Arc<Mutex<Vec<Log>>>) -> Launcher {
    let collected = Arc::clone(logs);
    Box::new(move |_server, root, _cancel| {
        let (client, log) = fake_client(root, fast());
        collected.lock().push(log);
        Ok(client)
    })
}

fn fake_config(format_on_write: bool) -> LspConfig {
    LspConfig {
        format_on_write,
        servers: BTreeMap::from([(
            "fake".to_string(),
            LspServerConfig {
                program: PathBuf::from("/bin/sh"),
                args: Some(vec![]),
                extensions: Some(vec!["fk".into()]),
                ..LspServerConfig::default()
            },
        )]),
        ..LspConfig::default()
    }
}

/// A tool that registers itself as the write-through owner, as the registered `lsp` tool does.
fn owner_tool(config: &LspConfig, logs: &Arc<Mutex<Vec<Log>>>) -> Arc<LspTool> {
    Arc::new_cyclic(|owner| {
        let mut tool = LspTool::with_launcher(config, recording_launcher(logs));
        tool.write_owner = Some(owner.clone());
        tool
    })
}

/// Runs `tool` against `ws` with approval granted for every mutation it asks about.
fn run_as(tool: &dyn Tool, ws: &Workspace, args: Value) -> ToolOutput {
    let config = Config::default();
    let authorize = |_: &str, _: &[PathBuf], _: bool| -> Result<(), String> { Ok(()) };
    tool.call(
        &ToolContext {
            workspace: ws,
            config: &config,
            mutation_authorizer: Some(&authorize),
            confirm: None,
        },
        &args,
    )
}

fn notices(log: &Log) -> usize {
    methods(log)
        .iter()
        .filter(|method| method.as_str() == "workspace/didChangeWatchedFiles")
        .count()
}

#[test]
fn write_through_reaches_only_the_session_that_executed_the_edit() {
    let dir = tempfile::tempdir().unwrap();
    let parent_ws = Workspace::new(dir.path()).unwrap();
    // A forked session over the same project has its own workspace instance and its own server.
    let child_ws = Workspace::new(dir.path()).unwrap();
    let logs: Arc<Mutex<Vec<Log>>> = Arc::default();
    let parent = owner_tool(&fake_config(false), &logs);
    let child = parent.fork_session().unwrap();
    let file = parent_ws.root().join("a.fk");
    std::fs::write(&file, "one\n").unwrap();
    let symbols = json!({"action": "document_symbols", "path": "a.fk"});
    assert!(!run_as(&*parent, &parent_ws, symbols.clone()).is_error);
    assert!(!run_as(&*child, &child_ws, symbols).is_error);
    wait_until("both sessions started a server", || logs.lock().len() == 2);
    let (parent_log, child_log) = {
        let logs = logs.lock();
        (Arc::clone(&logs[0]), Arc::clone(&logs[1]))
    };

    std::fs::write(&file, "one\ntwo\n").unwrap();
    writethrough(&parent_ws, &[(file.clone(), 2)], WriteOrigin::Edit);
    wait_until("parent notified of its edit", || notices(&parent_log) == 1);
    assert_eq!(
        notices(&child_log),
        0,
        "a parent edit reached the fork's server"
    );

    writethrough(&child_ws, &[(file.clone(), 2)], WriteOrigin::Edit);
    wait_until("fork notified of its edit", || notices(&child_log) == 1);
    assert_eq!(
        notices(&parent_log),
        1,
        "a fork edit reached the parent's server"
    );

    // Dropping the fork releases only its own binding and server; the parent keeps receiving edits.
    drop(child);
    writethrough(&parent_ws, &[(file.clone(), 2)], WriteOrigin::Edit);
    wait_until("parent still notified after the fork is dropped", || {
        notices(&parent_log) == 2
    });
}

#[test]
fn format_on_write_formats_once_and_observes_its_own_commit() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let logs: Arc<Mutex<Vec<Log>>> = Arc::default();
    let tool = owner_tool(&fake_config(true), &logs);
    let file = ws.root().join("a.fk");
    std::fs::write(&file, "one\n").unwrap();
    assert!(
        !run_as(
            &*tool,
            &ws,
            json!({"action": "document_symbols", "path": "a.fk"})
        )
        .is_error
    );
    std::fs::write(&file, "one\ntwo\n").unwrap();

    let notes = writethrough(&ws, &[(file.clone(), 2)], WriteOrigin::Edit);
    let log = Arc::clone(&logs.lock()[0]);
    let formats = methods(&log)
        .iter()
        .filter(|method| method.as_str() == "textDocument/formatting")
        .count();
    assert_eq!(
        formats, 1,
        "the formatter's own commit formatted again: {notes}"
    );
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "// formatted\none\ntwo\n"
    );
    assert!(!notes.contains("[LSP format"), "{notes}");
    // The formatted commit was still observed: the server was synchronized to the final text.
    wait_until("server sees the formatted text", || {
        log.lock().iter().any(|m| {
            m["method"] == "textDocument/didChange"
                && m["params"]["contentChanges"][0]["text"] == "// formatted\none\ntwo\n"
        })
    });
}

#[test]
fn edit_results_list_only_errors_the_edit_introduced() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let logs: Arc<Mutex<Vec<Log>>> = Arc::default();
    let tool = owner_tool(&fake_config(false), &logs);
    let file = ws.root().join("a.fk");
    std::fs::write(&file, "ERROR one\nfine\n").unwrap();
    let symbols = json!({"action": "document_symbols", "path": "a.fk"});
    assert!(!run_as(&*tool, &ws, symbols).is_error);
    let client = lock(&tool.running).values().next().cloned().unwrap();
    wait_until("the pre-edit publish", || {
        client.diagnostics_mark(&file) > 0
    });

    std::fs::write(&file, "ERROR one\nfine\nERROR three\n").unwrap();
    let notes = writethrough(&ws, &[(file.clone(), 2)], WriteOrigin::Edit);
    assert!(
        notes.contains(
            "[LSP advisory, not verification] 1 new error(s) after this edit:\na.fk:3:1: found ERROR\n"
        ),
        "{notes}"
    );
    assert!(
        !notes.contains("a.fk:1:1"),
        "a pre-existing error was reported: {notes}"
    );

    // Fewer errors than before: nothing new, nothing appended.
    std::fs::write(&file, "fine\nERROR one\n").unwrap();
    let notes = writethrough(&ws, &[(file.clone(), 2)], WriteOrigin::Edit);
    assert!(!notes.contains("new error"), "{notes}");

    // Many new errors are capped.
    let many: String = (0..11).map(|n| format!("ERROR {n}\n")).collect();
    std::fs::write(&file, many).unwrap();
    let notes = writethrough(&ws, &[(file.clone(), 2)], WriteOrigin::Edit);
    assert!(notes.contains("10 new error(s)"), "{notes}");
    assert_eq!(notes.matches(": found ERROR").count(), 8, "{notes}");
    assert!(notes.contains("... 2 more"), "{notes}");
    // The card and `dial watch` read the same block back.
    let (count, listed) = super::new_errors_in(&notes).unwrap();
    assert_eq!((count, listed.len()), (10, 8), "{notes}");
    assert!(listed.iter().all(|line| line.ends_with(": found ERROR")));
}

#[test]
fn new_errors_in_reads_back_the_advisory_block() {
    let note = super::new_errors_note(&["a.rs:1:2: bad".to_owned()], true);
    let text = format!("Edited a.rs\n{note}\nmore output");
    assert_eq!(super::new_errors_in(&text), Some((1, vec!["a.rs:1:2: bad"])));
    assert_eq!(super::new_errors_in("Edited a.rs\n"), None);
}

#[test]
fn edits_without_a_live_language_server_never_wait_for_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let logs: Arc<Mutex<Vec<Log>>> = Arc::default();
    let tool = owner_tool(&fake_config(false), &logs);
    // `status` binds the write-through owner without starting a server.
    assert!(!run_as(&*tool, &ws, json!({"action": "status"})).is_error);
    let file = ws.root().join("a.fk");
    std::fs::write(&file, "ERROR\n").unwrap();
    let started = Instant::now();
    let notes = writethrough(&ws, &[(file.clone(), 2)], WriteOrigin::Edit);
    assert!(notes.is_empty(), "{notes}");
    assert!(started.elapsed() < Duration::from_millis(500));
    assert!(logs.lock().is_empty(), "an edit started a language server");
}

#[test]
fn bounded_diagnostics_give_up_within_their_budget() {
    let (_dir, ws) = project();
    let (client, _log) = fake_client(ws.root(), fast());
    // Never synchronized, so the server never publishes for it.
    let path = ws.root().join("silent.fk");
    let started = Instant::now();
    let items = client.diagnostics_bounded(&path, 0, true, Duration::from_millis(200));
    assert!(items.is_none(), "{items:?}");
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn resource_operation_description_matches_the_approved_transactions() {
    let tool = LspTool::new(&LspConfig::default());
    let schema = tool.schema();
    let description = schema["function"]["description"].as_str().unwrap();
    assert!(description.contains(
        "Server-provided resource create/rename/delete operations are applied only by these approved calls"
    ));
    assert!(!description.contains("Resource create/rename/delete operations are refused"));
    assert!(tool.needs_approval(&json!({"action": "rename"})));
    assert!(tool.needs_approval(&json!({"action": "code_action_apply"})));
}

#[test]
fn rename_move_rollback_does_not_replace_a_recreated_source() {
    let (_dir, ws) = project();
    let source = ws.root().join("source");
    let target = ws.root().join("target");
    std::fs::write(&source, "original").unwrap();
    // The failed second move creates a parent at the first source name, simulating another
    // writer recreating that name after it was moved. Rollback must not replace even an empty dir.
    let error = move_units(&[
        (source.clone(), target.clone()),
        (ws.root().join("missing"), source.join("nested")),
    ])
    .unwrap_err();
    assert!(error.contains("could not restore"), "{error}");
    assert!(source.is_dir());
    assert_eq!(std::fs::read_to_string(target).unwrap(), "original");
}
