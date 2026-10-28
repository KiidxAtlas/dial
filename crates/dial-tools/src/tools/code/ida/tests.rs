#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::*;
use crate::tools::Workspace;
use crate::tools::integrations::ext::Concurrency;

/// Write a fake interpreter: a python3 script that ignores the worker source argv and runs `body`.
fn fake_python(dir: &Path, body: &str) -> String {
    let path = dir.join("fake-python");
    fs::write(
        &path,
        format!("#!/usr/bin/env python3\nimport json, sys\n{body}"),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

const WORKER: &str = r#"
def send(frame):
    sys.stdout.write(json.dumps(frame) + "\n")
    sys.stdout.flush()

send({"event": "ready"})
for line in sys.stdin:
    req = json.loads(line)
    rid, method, params = req["id"], req["method"], req.get("params", {})
    if method == "open":
        send({"id": rid, "ok": True, "result": {"idb": "/w/a.i64", "module": "a.bin", "format": "ELF", "arch": "arm", "bitness": 64}})
    elif method == "exec":
        code = params["code"]
        if code == "boom":
            send({"id": rid, "ok": True, "result": {"output": "", "value": None, "error": "Traceback: boom"}})
        elif code == "hang":
            for more in sys.stdin:
                if "interrupt" in json.loads(more):
                    send({"id": rid, "ok": True, "result": {"output": "", "value": None, "error": "KeyboardInterrupt: interrupted"}})
                    break
        elif code == "stuck":
            for _ in sys.stdin:
                pass
        elif code == "crash":
            sys.exit(7)
        else:
            send({"id": rid, "ok": True, "result": {"output": "hi\n", "value": "42", "error": None}})
    elif method == "close":
        send({"id": rid, "ok": True, "result": {"saved": params.get("save")}})
        break
    else:
        send({"id": rid, "ok": False, "error": {"type": "ValueError", "message": "unknown method"}})
"#;

fn ida_with(python: String) -> Ida {
    Ida::new(IdaConfig {
        python,
        ..IdaConfig::default()
    })
}

fn call(ida: &Ida, root: &Path, args: Value) -> ToolOutput {
    let workspace = Workspace::new(root).unwrap();
    let config = Config::default();
    let authorize = |_: &str, _: &[PathBuf], _: bool| -> Result<(), String> { Ok(()) };
    ida.call(
        &ToolContext {
            workspace: &workspace,
            config: &config,
            mutation_authorizer: Some(&authorize),
            confirm: None,
        },
        &args,
    )
}

fn sample_binary(root: &Path) {
    fs::write(root.join("a.bin"), "\x7fELF").unwrap();
}

#[test]
fn only_list_runs_without_approval_or_workspace_mutation() {
    let ida = ida_with(String::new());
    assert!(!ida.needs_approval(&json!({"action": "list"})));
    assert!(!ida.may_modify_workspace(&json!({"action": "list"})));
    assert!(matches!(
        ida.concurrency(&json!({"action": "list"})),
        Concurrency::Shared
    ));
    for action in ["open", "exec", "close", "save", "rename", "not-an-action"] {
        assert!(
            ida.needs_approval(&json!({"action": action})),
            "{action} must need approval"
        );
        assert!(matches!(
            ida.concurrency(&json!({"action": action})),
            Concurrency::Exclusive
        ));
    }
}

#[test]
fn missing_interpreter_names_the_setting() {
    let dir = tempfile::tempdir().unwrap();
    sample_binary(dir.path());
    let ida = ida_with(
        dir.path()
            .join("no-such-python")
            .to_string_lossy()
            .into_owned(),
    );
    let out = call(&ida, dir.path(), json!({"action": "open", "db": "a.bin"}));
    assert!(out.is_error);
    assert!(out.text.contains("ida.python"), "{}", out.text);
}

#[test]
fn startup_error_frame_reports_runtime_unusable() {
    let dir = tempfile::tempdir().unwrap();
    sample_binary(dir.path());
    let python = fake_python(
        dir.path(),
        "sys.stdout.write(json.dumps({'event': 'error', 'error': {'type': 'ImportError', 'message': 'no module named idapro'}}) + '\\n')\nsys.stdout.flush()\nsys.exit(3)\n",
    );
    let out = call(
        &ida_with(python),
        dir.path(),
        json!({"action": "open", "db": "a.bin"}),
    );
    assert!(out.is_error);
    assert!(
        out.text.contains("IDA runtime is not usable"),
        "{}",
        out.text
    );
    assert!(out.text.contains("no module named idapro"), "{}", out.text);
}

#[test]
fn open_exec_list_close_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    sample_binary(dir.path());
    let ida = ida_with(fake_python(dir.path(), WORKER));

    let opened = call(&ida, dir.path(), json!({"action": "open", "db": "a.bin"}));
    assert!(!opened.is_error, "{}", opened.text);
    assert!(opened.text.starts_with("Opened db1 "), "{}", opened.text);

    let listed = call(&ida, dir.path(), json!({"action": "list"}));
    assert!(
        listed.text.starts_with("db1  a.bin  ELF  arm/64  "),
        "{}",
        listed.text
    );

    let executed = call(
        &ida,
        dir.path(),
        json!({"action": "exec", "code": "print('hi')"}),
    );
    assert!(!executed.is_error, "{}", executed.text);
    assert_eq!(executed.text, "hi\n=> 42");

    let closed = call(&ida, dir.path(), json!({"action": "close", "db": "db1"}));
    assert!(!closed.is_error, "{}", closed.text);
    assert_eq!(closed.text, "Closed db1 (saved)");

    let after = call(&ida, dir.path(), json!({"action": "list"}));
    assert_eq!(after.text, "No IDA databases open.");
}

#[test]
fn open_requires_mutation_authorization() {
    let dir = tempfile::tempdir().unwrap();
    sample_binary(dir.path());
    let ida = ida_with(fake_python(dir.path(), WORKER));
    let workspace = Workspace::new(dir.path()).unwrap();
    let config = Config::default();
    let out = ida.call(
        &ToolContext {
            workspace: &workspace,
            config: &config,
            mutation_authorizer: None,
            confirm: None,
        },
        &json!({"action": "open", "db": "a.bin"}),
    );
    assert!(out.is_error, "{}", out.text);
    assert_eq!(ida.databases().len(), 0);
}

#[test]
fn exec_error_is_a_tool_error_and_keeps_the_database() {
    let dir = tempfile::tempdir().unwrap();
    sample_binary(dir.path());
    let ida = ida_with(fake_python(dir.path(), WORKER));
    call(&ida, dir.path(), json!({"action": "open", "db": "a.bin"}));
    let out = call(&ida, dir.path(), json!({"action": "exec", "code": "boom"}));
    assert!(out.is_error);
    assert!(out.text.contains("Traceback: boom"), "{}", out.text);
    assert_eq!(ida.databases().len(), 1);
}

#[test]
fn unknown_db_id_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    sample_binary(dir.path());
    let ida = ida_with(fake_python(dir.path(), WORKER));
    call(&ida, dir.path(), json!({"action": "open", "db": "a.bin"}));
    let out = call(
        &ida,
        dir.path(),
        json!({"action": "exec", "db": "db9", "code": "1"}),
    );
    assert!(out.is_error);
    assert!(out.text.contains("unknown db"), "{}", out.text);
}

#[test]
fn db_is_required_when_several_are_open() {
    let dir = tempfile::tempdir().unwrap();
    sample_binary(dir.path());
    fs::write(dir.path().join("b.bin"), "\x7fELF").unwrap();
    let ida = ida_with(fake_python(dir.path(), WORKER));
    call(&ida, dir.path(), json!({"action": "open", "db": "a.bin"}));
    call(&ida, dir.path(), json!({"action": "open", "db": "b.bin"}));
    let out = call(&ida, dir.path(), json!({"action": "save"}));
    assert!(out.is_error);
    assert!(out.text.contains("pass db"), "{}", out.text);
}

#[test]
fn exec_timeout_interrupts_and_keeps_the_database() {
    let dir = tempfile::tempdir().unwrap();
    sample_binary(dir.path());
    let ida = ida_with(fake_python(dir.path(), WORKER));
    call(&ida, dir.path(), json!({"action": "open", "db": "a.bin"}));
    let out = call(
        &ida,
        dir.path(),
        json!({"action": "exec", "code": "hang", "timeout": 1}),
    );
    assert!(out.is_error);
    assert!(out.text.contains("timed out after 1s"), "{}", out.text);
    assert_eq!(ida.databases().len(), 1);
}

#[test]
fn unresponsive_worker_is_killed_and_forgotten() {
    let dir = tempfile::tempdir().unwrap();
    sample_binary(dir.path());
    let ida = ida_with(fake_python(dir.path(), WORKER));
    call(&ida, dir.path(), json!({"action": "open", "db": "a.bin"}));
    let out = call(
        &ida,
        dir.path(),
        json!({"action": "exec", "code": "stuck", "timeout": 1}),
    );
    assert!(out.is_error);
    assert!(out.text.contains("was killed"), "{}", out.text);
    assert_eq!(ida.databases().len(), 0);
}

#[test]
fn worker_crash_is_reported_and_forgotten() {
    let dir = tempfile::tempdir().unwrap();
    sample_binary(dir.path());
    let ida = ida_with(fake_python(dir.path(), WORKER));
    call(&ida, dir.path(), json!({"action": "open", "db": "a.bin"}));
    let out = call(&ida, dir.path(), json!({"action": "exec", "code": "crash"}));
    assert!(out.is_error);
    assert!(out.text.contains("exited while exec"), "{}", out.text);
    assert_eq!(ida.databases().len(), 0);
}

#[test]
fn exec_timeout_out_of_range_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    sample_binary(dir.path());
    let ida = ida_with(fake_python(dir.path(), WORKER));
    call(&ida, dir.path(), json!({"action": "open", "db": "a.bin"}));
    let out = call(
        &ida,
        dir.path(),
        json!({"action": "exec", "code": "1", "timeout": 0}),
    );
    assert!(out.is_error);
    assert!(out.text.contains("timeout must be"), "{}", out.text);
}
