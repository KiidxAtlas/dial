use crate::config::tools::dap::DapAdapterConfig;
#[cfg(unix)]
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

#[test]
fn schemas_expose_runtime_probes_and_logpoint_arguments() {
    for tool in tools(&BTreeMap::new()) {
        let schema = tool.schema();
        let description = schema["function"]["description"].as_str().unwrap();
        for term in ["probes", "logpoints", "setBreakpoints", "logMessage"] {
            assert!(description.contains(term), "{} omits {term}", tool.name());
        }
        let help = schema["function"]["parameters"]["properties"]["breakpoints"]["description"]
            .as_str()
            .unwrap();
        for term in [
            "logMessage",
            "condition",
            "hitCondition",
            "supportsLogPoints",
        ] {
            assert!(
                help.contains(term),
                "{} breakpoint help omits {term}",
                tool.name()
            );
        }
    }
}

#[cfg(unix)]
fn fake_adapter(dir: &Path, mode: &str) -> DapAdapterConfig {
    let script = dir.join("adapter.py");
    fs::write(&script, r#"#!/usr/bin/env python3
import json,os,sys,time
restarted=False
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
 def emit(b):
  out=json.dumps(b,separators=(',',':')).encode()
  sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n'%len(out)).encode()+out);sys.stdout.buffer.flush()
 cmd=req['command']
 rmode=os.environ.get('DAP_FAKE_RESTART','') if cmd=='restart' else ''
 if cmd=='launch' and os.environ.get('DAP_FAKE_LAUNCH_INIT')=='1': emit({'seq':9,'type':'event','event':'initialized','body':{}})
 if rmode=='stall': time.sleep(30)
 if rmode in ('initialized','delayed'): emit({'seq':9,'type':'event','event':'initialized','body':{}})
 if rmode=='delayed': time.sleep(0.3)
 body={'seq':1,'type':'response','request_seq':req['seq'],'command':cmd,'success':True,'body':{'received':cmd,'args':req.get('arguments',{})}}
 if cmd=='initialize': body['body'].update(json.loads(os.environ.get('DAP_FAKE_CAPS','{}')))
 if rmode=='reject':
  body['success']=False; body['message']='restart refused'
 if cmd=='restart': restarted=True
 if cmd=='configurationDone' and restarted and os.environ.get('DAP_FAKE_REJECT_LATE_CONFIG')=='1':
  body['success']=False; body['message']='configurationDone refused after restart'
 emit(body)
 if cmd in ('disconnect','terminate'): sys.exit(0)
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

/// The fake adapter, advertising `caps` in its `initialize` response.
#[cfg(unix)]
fn fake_adapter_with_caps(dir: &Path, caps: &Value) -> DapAdapterConfig {
    let mut config = fake_adapter(dir, "valid");
    config
        .env
        .insert("DAP_FAKE_CAPS".to_owned(), caps.to_string());
    config
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

#[cfg(unix)]
#[test]
fn unsupported_probe_options_are_refused_without_poisoning_session() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let configs = BTreeMap::from([("fake".to_owned(), fake_adapter(dir.path(), "valid"))]);
    let tool = pick(tools(&configs), "dap_fake");
    let config = crate::config::types::Config::default();
    let ctx = context(&workspace, &config);
    let source = json!({"path": "/src/main.rs"});

    // This adapter advertises no optional breakpoint capabilities.
    for (field, value) in [("logMessage", "value={value}"), ("hitCondition", "3")] {
        let mut breakpoint = json!({"line": 20});
        breakpoint[field] = json!(value);
        let refused = tool.call(
            &ctx,
            &json!({"action":"setBreakpoints","source":source,"breakpoints":[breakpoint]}),
        );
        assert!(refused.is_error, "{}", refused.text);
        assert!(
            refused
                .text
                .contains(&format!("adapter does not support breakpoint {field}")),
            "{}",
            refused.text
        );
    }

    let ordinary = tool.call(
        &ctx,
        &json!({"action":"setBreakpoints","source":source,"breakpoints":[{"line":5}]}),
    );
    assert!(!ordinary.is_error, "{}", ordinary.text);
    let response: Value = serde_json::from_str(&ordinary.text).unwrap();
    assert_eq!(response["args"]["breakpoints"], json!([{"line":5}]));
}

#[test]
fn exception_and_restart_arguments_are_validated_before_any_session_starts() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let config = crate::config::types::Config::default();
    let ctx = context(&workspace, &config);
    let tool = pick(tools(&BTreeMap::new()), "dap");
    for (args, expected) in [
        (
            json!({"action":"setExceptionBreakpoints"}),
            "requires an array of string filters",
        ),
        (
            json!({"action":"setExceptionBreakpoints","filters":[1]}),
            "requires an array of string filters",
        ),
        (
            json!({"action":"setExceptionBreakpoints","filters":[],"arguments":{}}),
            "not arguments",
        ),
        (
            json!({"action":"setExceptionBreakpoints","filters":["raised"],"filterOptions":[{"condition":"x"}]}),
            "filterId",
        ),
        (
            json!({"action":"setExceptionBreakpoints","filters":["raised"],"exceptionOptions":[{"breakMode":"sometimes"}]}),
            "breakMode",
        ),
        (
            json!({"action":"restart","arguments":{"program":"/bin/true"}}),
            "restart takes no arguments",
        ),
    ] {
        let output = tool.call(&ctx, &args);
        assert!(
            output.is_error,
            "{args} unexpectedly passed: {}",
            output.text
        );
        assert!(output.text.contains(expected), "{args}: {}", output.text);
    }
}

#[test]
fn exception_and_restart_actions_are_schema_visible() {
    let schema = tools(&BTreeMap::new())
        .into_iter()
        .find(|tool| tool.name() == "dap")
        .unwrap()
        .schema();
    let properties = &schema["function"]["parameters"]["properties"];
    let actions: Vec<&str> = properties["action"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    for action in ["setExceptionBreakpoints", "exceptionInfo", "restart"] {
        assert!(actions.contains(&action), "schema omits {action}");
    }
    for property in ["filters", "filterOptions", "exceptionOptions"] {
        assert!(
            properties.get(property).is_some(),
            "schema omits {property}"
        );
    }
}

#[cfg(unix)]
#[test]
fn exception_breakpoints_and_exception_info_reach_the_wire_with_validated_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let caps = json!({
        "exceptionBreakpointFilters": [{"filter": "raised", "supportsCondition": true}],
        "supportsExceptionFilterOptions": true,
        "supportsExceptionOptions": true,
        "supportsExceptionInfoRequest": true,
    });
    let configs = BTreeMap::from([("fake".to_owned(), fake_adapter_with_caps(dir.path(), &caps))]);
    let tool = pick(tools(&configs), "dap_fake");
    let config = crate::config::types::Config::default();
    let ctx = context(&workspace, &config);

    let set = tool.call(
        &ctx,
        &json!({"action":"setExceptionBreakpoints","filters":["raised"],
            "filterOptions":[{"filterId":"raised","condition":"x > 1","hitCondition":null}],
            "exceptionOptions":[{"breakMode":"unhandled","path":[{"names":["Error"]}]}]}),
    );
    assert!(!set.is_error, "{}", set.text);
    let sent: Value = serde_json::from_str(&set.text).unwrap();
    assert_eq!(sent["args"]["filters"], json!(["raised"]));
    assert_eq!(
        sent["args"]["filterOptions"],
        json!([{"filterId":"raised","condition":"x > 1"}]),
        "null option fields must not reach the adapter"
    );
    assert_eq!(
        sent["args"]["exceptionOptions"][0]["breakMode"],
        "unhandled"
    );

    let status: Value =
        serde_json::from_str(&tool.call(&ctx, &json!({"action":"status"})).text).unwrap();
    assert_eq!(status["exceptionBreakpoints"], sent["args"]);

    let info = tool.call(&ctx, &json!({"action":"exceptionInfo","threadId":4}));
    assert!(!info.is_error, "{}", info.text);
    let info: Value = serde_json::from_str(&info.text).unwrap();
    assert_eq!(info["received"], "exceptionInfo");
    assert_eq!(info["args"], json!({"threadId": 4}));
}

#[cfg(unix)]
#[test]
fn exception_requests_refuse_what_the_adapter_did_not_advertise() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("program"), "").unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    // Filter options are advertised, conditions are not per filter; no exception options,
    // exception info, or restart support.
    let caps = json!({
        "exceptionBreakpointFilters": [{"filter": "raised", "supportsCondition": false}],
        "supportsExceptionFilterOptions": true,
    });
    let configs = BTreeMap::from([("fake".to_owned(), fake_adapter_with_caps(dir.path(), &caps))]);
    let tool = pick(tools(&configs), "dap_fake");
    let config = crate::config::types::Config::default();
    let ctx = context(&workspace, &config);
    let launched = tool.call(&ctx, &json!({"action":"launch","program":"program"}));
    assert!(!launched.is_error, "{}", launched.text);

    for (args, expected) in [
        (
            json!({"action":"setExceptionBreakpoints","filters":["uncaught"]}),
            "does not advertise exception filter \"uncaught\"",
        ),
        (
            json!({"action":"setExceptionBreakpoints","filters":[],"filterOptions":[{"filterId":"raised","condition":"x"}]}),
            "does not support conditions on exception filter \"raised\"",
        ),
        (
            json!({"action":"setExceptionBreakpoints","filters":["raised"],"exceptionOptions":[{"breakMode":"always"}]}),
            "does not support exception options",
        ),
        (
            json!({"action":"exceptionInfo","threadId":1}),
            "does not support exceptionInfo",
        ),
        (json!({"action":"restart"}), "does not support restart"),
    ] {
        let output = tool.call(&ctx, &args);
        assert!(
            output.is_error,
            "{args} unexpectedly succeeded: {}",
            output.text
        );
        assert!(output.text.contains(expected), "{args}: {}", output.text);
    }
}

#[cfg(unix)]
#[test]
fn restart_without_initialized_completes_configuration() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("program"), "").unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let configs = BTreeMap::from([(
        "fake".to_owned(),
        fake_adapter_with_caps(dir.path(), &json!({"supportsRestartRequest": true})),
    )]);
    let tool = pick(tools(&configs), "dap_fake");
    let config = crate::config::types::Config::default();
    let ctx = context(&workspace, &config);
    let launched = tool.call(&ctx, &json!({"action":"launch","program":"program"}));
    assert!(!launched.is_error, "{}", launched.text);

    let restarted = tool.call(&ctx, &json!({"action":"restart"}));
    assert!(!restarted.is_error, "{}", restarted.text);
    let body: Value = serde_json::from_str(&restarted.text).unwrap();
    assert_eq!(body["response"]["received"], "restart");
    // A successful answer without `initialized` completes configuration; nothing is due.
    assert_eq!(body["session"]["needsConfigurationDone"], false);
    assert_eq!(body["session"]["configurationDone"], true);
    // Restart keeps the session alive and never reports the old debuggee as stopped.
    let status: Value =
        serde_json::from_str(&tool.call(&ctx, &json!({"action":"status"})).text).unwrap();
    assert_eq!(status["active"], true);
    assert_eq!(status["stopped"], Value::Null);
}

/// The launchable fake adapter for restart tests. It advertises restart and configurationDone and
/// reports `initialized` before its launch answer. `restart_mode` shapes its answer to `restart`:
/// "" answers success only, "initialized" reports `initialized` then answers, "delayed" pauses
/// before answering, "reject" fails, and "stall" never answers.
#[cfg(unix)]
fn restart_adapter(dir: &Path, restart_mode: &str, timeout_secs: u64) -> DapAdapterConfig {
    let mut config = fake_adapter_with_caps(
        dir,
        &json!({"supportsRestartRequest": true, "supportsConfigurationDoneRequest": true}),
    );
    config
        .env
        .insert("DAP_FAKE_LAUNCH_INIT".to_owned(), "1".to_owned());
    config
        .env
        .insert("DAP_FAKE_RESTART".to_owned(), restart_mode.to_owned());
    config.timeout_secs = timeout_secs;
    config
}

/// The restart test adapter as a dap tool.
#[cfg(unix)]
fn restart_tool(dir: &Path, restart_mode: &str, timeout_secs: u64) -> Arc<dyn Tool> {
    pick(
        tools(&BTreeMap::from([(
            "fake".to_owned(),
            restart_adapter(dir, restart_mode, timeout_secs),
        )])),
        "dap_fake",
    )
}

/// Launches and completes the initial configuration, so restart starts from a configured session.
#[cfg(unix)]
fn configured_restart_session(tool: &Arc<dyn Tool>, ctx: &ToolContext) {
    let launched = tool.call(ctx, &json!({"action":"launch","program":"program"}));
    assert!(!launched.is_error, "{}", launched.text);
    let done = tool.call(ctx, &json!({"action":"configurationDone"}));
    assert!(!done.is_error, "{}", done.text);
}

#[cfg(unix)]
#[test]
fn restart_configuration_gate_accepts_initialized_or_a_successful_answer() {
    // The answer to restart is the only proof of readiness for "" (it is never delayed), so it
    // must surface there; the other modes may surface it at restart or at configurationDone.
    for (mode, answered_at_restart) in [("", true), ("initialized", false), ("delayed", false)] {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("program"), "").unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        let config = crate::config::types::Config::default();
        let ctx = context(&workspace, &config);
        let tool = restart_tool(dir.path(), mode, 2);
        configured_restart_session(&tool, &ctx);

        let restarted = tool.call(&ctx, &json!({"action":"restart"}));
        assert!(!restarted.is_error, "{mode}: {}", restarted.text);
        let restart_body: Value = serde_json::from_str(&restarted.text).unwrap();
        let restart_answered = restart_body["response"]["received"] == "restart";
        assert_eq!(
            restart_body["session"]["needsConfigurationDone"], !answered_at_restart,
            "{mode}"
        );
        if answered_at_restart {
            assert!(restart_answered, "{mode}: {}", restarted.text);
        }

        // Breakpoints must not wait for a second `initialized` once the restart is answered.
        let source = json!({"path": "/src/main.rs"});
        let breakpoints = tool.call(
            &ctx,
            &json!({"action":"setBreakpoints","source":source,"breakpoints":[{"line":3}]}),
        );
        assert!(!breakpoints.is_error, "{mode}: {}", breakpoints.text);

        let done = tool.call(&ctx, &json!({"action":"configurationDone"}));
        assert!(!done.is_error, "{mode}: {}", done.text);
        let done_body: Value = serde_json::from_str(&done.text).unwrap();
        assert!(
            restart_answered || done_body["response"]["received"] == "restart",
            "{mode}: restart answer lost: {} / {}",
            restarted.text,
            done.text
        );
        let status: Value =
            serde_json::from_str(&tool.call(&ctx, &json!({"action":"status"})).text).unwrap();
        assert_eq!(status["configurationDone"], true, "{mode}");
        assert_eq!(status["needsConfigurationDone"], false, "{mode}");
    }
}

#[cfg(unix)]
#[test]
fn silent_restart_skips_configuration_done_so_a_refusing_adapter_still_resumes() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("program"), "").unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let config = crate::config::types::Config::default();
    let ctx = context(&workspace, &config);
    let mut adapter = restart_adapter(dir.path(), "", 2);
    adapter
        .env
        .insert("DAP_FAKE_REJECT_LATE_CONFIG".to_owned(), "1".to_owned());
    let tool = pick(
        tools(&BTreeMap::from([("fake".to_owned(), adapter)])),
        "dap_fake",
    );
    configured_restart_session(&tool, &ctx);

    let restarted = tool.call(&ctx, &json!({"action":"restart"}));
    assert!(!restarted.is_error, "{}", restarted.text);
    let status: Value =
        serde_json::from_str(&tool.call(&ctx, &json!({"action":"status"})).text).unwrap();
    assert_eq!(status["configurationDone"], true);
    assert_eq!(status["needsConfigurationDone"], false);

    // The adapter refuses configurationDone after restart. A silent restart never sends one, so
    // the next execution action still reaches the adapter.
    let resumed = tool.call(&ctx, &json!({"action":"continue","threadId":1}));
    assert!(!resumed.is_error, "{}", resumed.text);
    let body: Value = serde_json::from_str(&resumed.text).unwrap();
    assert_eq!(body["received"], "continue");
}

#[cfg(unix)]
#[test]
fn rejected_restart_restores_the_configured_session() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("program"), "").unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let config = crate::config::types::Config::default();
    let ctx = context(&workspace, &config);
    let tool = restart_tool(dir.path(), "reject", 2);
    configured_restart_session(&tool, &ctx);

    let restarted = tool.call(&ctx, &json!({"action":"restart"}));
    assert!(restarted.is_error, "{}", restarted.text);
    assert!(
        restarted.text.contains("restart refused"),
        "{}",
        restarted.text
    );
    let status: Value =
        serde_json::from_str(&tool.call(&ctx, &json!({"action":"status"})).text).unwrap();
    assert_eq!(status["active"], true);
    assert_eq!(status["configurationDone"], true);
    assert_eq!(status["needsConfigurationDone"], false);
}

#[cfg(unix)]
#[test]
fn unanswered_restart_times_out_and_cancellation_interrupts_it() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("program"), "").unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let config = crate::config::types::Config::default();
    let ctx = context(&workspace, &config);

    // This deadline also covers Python adapter startup under a parallel workspace test run.
    let timed_out = restart_tool(dir.path(), "stall", 5);
    configured_restart_session(&timed_out, &ctx);
    let restarted = timed_out.call(&ctx, &json!({"action":"restart"}));
    assert!(restarted.is_error, "{}", restarted.text);
    assert!(restarted.text.contains("timed out"), "{}", restarted.text);

    let cancelled = restart_tool(dir.path(), "stall", 5);
    configured_restart_session(&cancelled, &ctx);
    let flag = Arc::new(AtomicBool::new(false));
    let setter = {
        let flag = Arc::clone(&flag);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            flag.store(true, Ordering::Release);
        })
    };
    let restarted = cancelled.call_streaming(&ctx, &json!({"action":"restart"}), &|_| {}, &flag);
    setter.join().unwrap();
    assert!(restarted.is_error, "{}", restarted.text);
    assert!(restarted.text.contains("cancelled"), "{}", restarted.text);
}
