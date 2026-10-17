use crate::config::tools::dap::DapAdapterConfig;
use crate::config::tools::dap::DapTransport;

use super::*;
#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};

#[test]
fn adapter_config_deserializes_defaults_and_rejects_unknown_fields() {
    let config: DapAdapterConfig = toml::from_str("program = 'adapter'").unwrap();
    assert_eq!(config.program, PathBuf::from("adapter"));
    assert!(config.args.is_empty());
    assert!(config.env.is_empty());
    assert_eq!(config.timeout_secs, 0);
    assert!(toml::from_str::<DapAdapterConfig>("program = 'adapter'\nunknown = true").is_err());
}

#[cfg(unix)]
fn fake_adapter(dir: &Path, mode: &str) -> DapAdapterConfig {
    let script = dir.join("adapter.py");
    fs::write(&script, r#"#!/usr/bin/env python3
import json,os,sys,time
while True:
 h={}
 while True:
  line=sys.stdin.buffer.readline()
  if not line: sys.exit(0)
  if line in (b'\r\n',b'\n'): break
  k,v=line.decode().split(':',1); h[k.lower()]=v.strip()
 n=int(h['content-length']); req=json.loads(sys.stdin.buffer.read(n))
 mode=os.environ.get('DAP_FAKE_MODE','')
 if mode == 'invalid':
  sys.stdout.buffer.write(b'Content-Length: 1\r\n\r\n{');sys.stdout.buffer.flush();sys.exit(0)
 if mode == 'oversized':
  sys.stdout.buffer.write(b'Content-Length: 999999999\r\n\r\n');sys.stdout.buffer.flush();sys.exit(0)
 if mode == 'timeout': time.sleep(5)
 body={'seq':1,'type':'response','request_seq':req['seq'],'command':req['command'],'success':True,'body':{'received':req['command'],'args':req.get('arguments',{})}}
 out=json.dumps(body,separators=(',',':')).encode()
 sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n'%len(out)).encode()+out);sys.stdout.buffer.flush()
 if req['command'] in ('disconnect','terminate'): sys.exit(0)
"#).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    DapAdapterConfig {
        program: PathBuf::from("python3"),
        args: vec![script.to_string_lossy().into_owned()],
        env: BTreeMap::from([("DAP_FAKE_MODE".to_owned(), mode.to_owned())]),
        timeout_secs: if mode == "timeout" { 1 } else { 2 },
        transport: DapTransport::Stdio,
        address: String::new(),
        file_types: Vec::new(),
        root_markers: Vec::new(),
        launch_defaults: serde_json::Map::new(),
        attach_defaults: serde_json::Map::new(),
    }
}

#[cfg(unix)]
fn context<'a>(
    workspace: &'a Workspace,
    config: &'a crate::config::types::Config,
) -> ToolContext<'a> {
    ToolContext {
        workspace,
        config,
        mutation_authorizer: None,
        confirm: None,
    }
}

#[cfg(unix)]
fn pick(tools: Vec<Arc<dyn Tool>>, name: &str) -> Arc<dyn Tool> {
    tools
        .into_iter()
        .find(|tool| tool.name() == name)
        .unwrap_or_else(|| panic!("no tool named {name}"))
}

#[cfg(unix)]
#[test]
fn fake_adapter_exercises_framing_lifecycle_and_workspace_validation() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("program"), "").unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let mut configs = BTreeMap::new();
    configs.insert("fake".to_owned(), fake_adapter(dir.path(), "valid"));
    let tool = pick(tools(&configs), "dap_fake");
    let config = crate::config::types::Config::default();
    let ctx = context(&workspace, &config);
    assert_eq!(tool.name(), "dap_fake");
    assert!(tool.needs_approval(&json!({"action":"stackTrace"})));
    let init = tool.call(&ctx, &json!({"action":"initialize"}));
    assert!(!init.is_error, "{}", init.text);
    assert!(init.text.contains("\"adapterID\":\"fake\""));
    let launch = tool.call(&ctx, &json!({"action":"launch","program":"program"}));
    assert!(!launch.is_error, "{}", launch.text);
    assert!(launch.text.contains("\"cwd\":"));
    let bad = tool.call(&ctx, &json!({"action":"launch","program":"../outside"}));
    assert!(bad.is_error);
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), dir.path().join("escape")).unwrap();
    let symlink_escape = tool.call(&ctx, &json!({"action":"launch","program":"escape"}));
    assert!(symlink_escape.is_error);
    let ended = tool.call(&ctx, &json!({"action":"disconnect"}));
    assert!(!ended.is_error, "{}", ended.text);
}

#[cfg(unix)]
#[test]
fn fake_adapter_protocol_failures_stop_the_session() {
    for mode in ["invalid", "oversized", "timeout"] {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        let mut configs = BTreeMap::new();
        configs.insert("fake".to_owned(), fake_adapter(dir.path(), mode));
        let tool = pick(tools(&configs), "dap_fake");
        let config = crate::config::types::Config::default();
        let ctx = context(&workspace, &config);
        let output = tool.call(&ctx, &json!({"action":"initialize"}));
        assert!(
            output.is_error,
            "{mode} adapter unexpectedly succeeded: {}",
            output.text
        );
    }
}

#[cfg(unix)]
#[test]
fn breakpoint_actions_merge_into_the_session_book() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let mut configs = BTreeMap::new();
    configs.insert("fake".to_owned(), fake_adapter(dir.path(), "valid"));
    let tool = pick(tools(&configs), "dap_fake");
    let config = crate::config::types::Config::default();
    let ctx = context(&workspace, &config);
    let source = json!({"path": "/src/main.rs"});

    let first = tool.call(
        &ctx,
        &json!({"action":"setBreakpoints","source":source,"breakpoints":[{"line":20}]}),
    );
    assert!(!first.is_error, "{}", first.text);
    let second = tool.call(
        &ctx,
        &json!({"action":"setBreakpoints","source":source,"breakpoints":[{"line":5}]}),
    );
    assert!(!second.is_error, "{}", second.text);
    assert!(
        second.text.contains(r#"[{"line":5},{"line":20}]"#),
        "{}",
        second.text
    );

    let removed = tool.call(
        &ctx,
        &json!({"action":"removeBreakpoint","source":source,"line":5}),
    );
    assert!(!removed.is_error, "{}", removed.text);
    assert!(
        removed.text.contains(r#""breakpoints":[{"line":20}]"#),
        "{}",
        removed.text
    );

    let status = tool.call(&ctx, &json!({"action":"status"}));
    assert!(
        status.text.contains(r#""breakpoints""#) && status.text.contains("/src/main.rs"),
        "{}",
        status.text
    );

    let added = tool.call(
        &ctx,
        &json!({"action":"setFunctionBreakpoints","breakpoints":[{"name":"other"}]}),
    );
    assert!(!added.is_error, "{}", added.text);
    let added = tool.call(
        &ctx,
        &json!({"action":"setFunctionBreakpoints","breakpoints":[{"name":"main"}]}),
    );
    assert!(!added.is_error, "{}", added.text);
    assert!(
        added.text.contains(r#"[{"name":"main"},{"name":"other"}]"#),
        "{}",
        added.text
    );
    let dropped = tool.call(
        &ctx,
        &json!({"action":"removeFunctionBreakpoint","name":"main"}),
    );
    assert!(!dropped.is_error, "{}", dropped.text);
    assert!(
        dropped.text.contains(r#""breakpoints":[{"name":"other"}]"#),
        "{}",
        dropped.text
    );

    let invalid = tool.call(&ctx, &json!({"action":"removeBreakpoint","source":source}));
    assert!(invalid.is_error);
}
