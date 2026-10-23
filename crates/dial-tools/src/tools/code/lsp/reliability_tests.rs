//! Process-backed regressions for request settlement and lifecycle ownership.
use super::*;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

fn timeouts() -> Timeouts {
    Timeouts {
        initialize: Duration::from_secs(30),
        request: Duration::from_millis(150),
        shutdown: Duration::from_millis(100),
        diagnostics: Duration::from_millis(100),
        settle: Duration::from_millis(10),
    }
}

fn wait_for(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "fixture did not reach expected state"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn call(tool: &LspTool, ws: &Workspace, args: Value) -> ToolOutput {
    let config = Config::default();
    let authorize = crate::tools::testkit::authorize_in_workspace(ws, "lsp");
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

// Shell plus standard POSIX utilities: no provider accounts, installed language servers or Python.
fn fixture(root: &Path, mode: &str) -> Client {
    let command = match mode {
        "timeout" => "sleep 30",
        "disconnect" => "exit 0",
        "error" => {
            "send '{\"jsonrpc\":\"2.0\",\"id\":2,\"error\":{\"code\":-32800,\"message\":\"terminal cancellation\"}}'"
        }
        "retry" => {
            "send '{\"jsonrpc\":\"2.0\",\"id\":2,\"error\":{\"code\":-32802,\"message\":\"server cancelled\"}}'"
        }
        "success" => "send '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":null}'",
        _ => panic!("unknown fixture mode"),
    };
    let script = format!(
        r#"
send() {{ printf 'Content-Length: %s\r\n\r\n%s' "${{#1}}" "$1"; }}
while IFS= read -r header; do
    length=$(printf '%s' "${{header#Content-Length: }}" | tr -d '\r')
    IFS= read -r blank || exit 0
    body=$(dd bs=1 count="$length" 2>/dev/null)
    case "$body" in
        *'"method":"initialize"'*) send '{{"jsonrpc":"2.0","id":1,"result":{{"capabilities":{{}}}}}}' ;;
        *'"method":"workspace/executeCommand"'*) echo command >> commands; {command} ;;
        *'"method":"shutdown"'*) send '{{"jsonrpc":"2.0","id":3,"result":null}}' ;;
        *'"method":"exit"'*) exit 0 ;;
    esac
done
"#
    );
    Client::spawn(
        "fixture",
        Path::new("/bin/sh"),
        &["-c".into(), script],
        root,
        timeouts(),
        ServerOptions::default(),
    )
    .unwrap()
}

#[test]
fn execute_command_preserves_typed_settlement_and_never_retries() {
    for (mode, unknown, failed) in [
        ("timeout", true, true),
        ("disconnect", true, true),
        ("error", false, true),
        ("retry", false, true),
        ("success", false, false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        let path = ws.root().join("a.fk");
        std::fs::write(&path, "fn main() {}\n").unwrap();
        let client = Arc::new(fixture(ws.root(), mode));
        let mut config = LspConfig::default();
        config.servers.insert(
            "fake".into(),
            crate::config::tools::lsp::LspServerConfig {
                program: PathBuf::from("/bin/sh"),
                extensions: Some(vec!["fk".into()]),
                ..Default::default()
            },
        );
        let tool = LspTool::new(&config);
        lock(&tool.actions).insert(
            "fixture".into(),
            CachedAction {
                client,
                source: path,
                language: "fake".into(),
                snapshot: Arc::new(Snapshot::capture(&ws).unwrap()),
                action: json!({"title": "fixture", "command": "fixture.command"}),
                applied: false,
                command_executed: false,
            },
        );
        let args = json!({"action": "execute_command", "path": "a.fk", "action_id": "fixture"});
        let output = call(&tool, &ws, args);
        assert_eq!(output.is_error, failed, "{mode}");
        assert_eq!(
            output.execution.effects == Effects::Unknown,
            unknown,
            "{mode}"
        );
        assert_eq!(
            std::fs::read_to_string(ws.root().join("commands"))
                .unwrap()
                .lines()
                .count(),
            1,
            "{mode}"
        );
        assert!(lock(&tool.actions)["fixture"].command_executed);
    }
}

#[test]
fn cancelling_silent_initialization_terminates_and_reaps_pending_server() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    std::fs::write(ws.root().join("a.fk"), "x\n").unwrap();
    let mut config = LspConfig::default();
    config.servers.insert(
        "fake".into(),
        crate::config::tools::lsp::LspServerConfig {
            program: PathBuf::from("/bin/sh"),
            args: Some(vec!["-c".into(), "echo $$ > started; exec sleep 30".into()]),
            extensions: Some(vec!["fk".into()]),
            ..Default::default()
        },
    );
    let tool = LspTool::new(&config);
    thread::scope(|scope| {
        let worker = scope.spawn(|| {
            call(
                &tool,
                &ws,
                json!({"action":"hover", "path":"a.fk", "line":1, "column":1}),
            )
        });
        wait_for(|| ws.root().join("started").exists());
        let start = Instant::now();
        tool.cancel_operation();
        let output = worker.join().unwrap();
        assert!(output.is_error);
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(lock(&tool.running).is_empty());
    });
    let pid = std::fs::read_to_string(ws.root().join("started")).unwrap();
    assert!(!running(pid.trim()));
}

fn running(pid: &str) -> bool {
    let output = Command::new("ps")
        .args(["-o", "stat=", "-p", pid])
        .output()
        .unwrap();
    let state = String::from_utf8_lossy(&output.stdout);
    !state.trim().is_empty() && !state.trim_start().starts_with('Z')
}

#[test]
fn stop_cleans_descendants_after_group_leader_exits() {
    let dir = tempfile::tempdir().unwrap();
    let reply = r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#;
    let script = format!(
        r#"
sleep 30 <&0 &
echo $! > descendant
printf 'Content-Length: %s\r\n\r\n%s' '{}' '{}'
while [ ! -f release ]; do sleep 0.01; done
exit 0
"#,
        reply.len(),
        reply
    );
    let client = Client::spawn(
        "fixture",
        Path::new("/bin/sh"),
        &["-c".into(), script],
        dir.path(),
        timeouts(),
        ServerOptions::default(),
    )
    .unwrap();
    let leader = client.status()["process_id"].as_u64().unwrap().to_string();
    std::fs::write(dir.path().join("release"), "").unwrap();
    wait_for(|| !running(&leader));
    let descendant = std::fs::read_to_string(dir.path().join("descendant")).unwrap();
    assert!(running(descendant.trim()));
    client.stop();
    wait_for(|| !running(descendant.trim()));
    assert!(!running(&leader));
}
