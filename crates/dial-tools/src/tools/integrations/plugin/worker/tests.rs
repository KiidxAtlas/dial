use super::super::*;

const DRIVER: &str = r#"
import sys,json,os,time
n=0
callback_id=0
for line in sys.stdin:
    request=json.loads(line)
    method=request['method']
    if method=='initialize':
        result={'protocolVersion':'2','commands':['count']}
    elif method=='count':
        n+=1
        result=n
    elif method=='crash':
        os._exit(3)
    elif method=='malformed':
        print('not json',flush=True)
        continue
    elif method=='error':
        print(json.dumps({'id':request['id'],'result':{'response':{'error':{'code':-1,'message':'expected failure'}}}}),flush=True)
        continue
    elif method=='callback':
        callback_id+=1
        print(json.dumps({'callback':{'method':'tools.list','params':{}},'operation_id':request['id'],'callback_id':callback_id}),flush=True)
        result=json.loads(sys.stdin.readline())
    elif method=='flood':
        for _ in range(257):
            callback_id+=1
            print(json.dumps({'callback':{'method':'progress','params':{'text':'tick'}},'operation_id':request['id'],'callback_id':callback_id}),flush=True)
            json.loads(sys.stdin.readline())
        result=None
    elif method=='wait':
        time.sleep(30)
        result=None
    else:
        result=None
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':{'response':{'result':result}}}),flush=True)
"#;
fn process(mode: &str) -> ProcessPlugin {
    ProcessPlugin::start(
        "worker",
        Path::new("python3"),
        &["-u".into(), "-c".into(), DRIVER.into()],
        &BTreeMap::from([("DIAL_EXTENSION_RUNTIME".into(), mode.into())]),
        2,
    )
    .unwrap()
}
fn call(plugin: &dyn Plugin, method: &str) -> anyhow::Result<Value> {
    plugin.call(method, Value::Null, &AtomicBool::new(false))
}

#[test]
fn persistent_state_fork_and_explicit_recovery() {
    let plugin = process("worker");
    assert_eq!(call(&plugin, "count").unwrap(), 1);
    assert_eq!(call(&plugin, "count").unwrap(), 2);
    let child = plugin.fork_session().unwrap();
    assert_eq!(call(child.as_ref(), "count").unwrap(), 1);
    assert!(
        call(&plugin, "error")
            .unwrap_err()
            .to_string()
            .contains("expected failure")
    );
    assert_eq!(call(&plugin, "count").unwrap(), 3);
    for failure in ["crash", "malformed"] {
        assert!(call(&plugin, failure).is_err());
        assert!(
            call(&plugin, "count")
                .unwrap_err()
                .to_string()
                .contains("explicitly restart")
        );
        plugin.restart(&AtomicBool::new(false)).unwrap();
        assert_eq!(call(&plugin, "count").unwrap(), 1);
    }
    assert_eq!(call(child.as_ref(), "count").unwrap(), 2);
    plugin.shutdown();
    assert!(call(&plugin, "count").is_err());
    assert_eq!(call(child.as_ref(), "count").unwrap(), 3);
}

#[test]
fn request_mode_is_stateless_and_callbacks_are_bounded_and_nonreentrant() {
    let one = process("request");
    assert_eq!(call(&one, "count").unwrap(), 1);
    assert_eq!(call(&one, "count").unwrap(), 1);
    let plugin = process("worker");
    for _ in 0..2 {
        let value = plugin
            .call_with_host(
                "callback",
                Value::Null,
                &AtomicBool::new(false),
                &mut |method, params| {
                    assert_eq!(method, "tools.list");
                    assert_eq!(params, &json!({}));
                    assert!(
                        call(&plugin, "count")
                            .unwrap_err()
                            .to_string()
                            .contains("busy")
                    );
                    Ok(json!(["approved"]))
                },
            )
            .unwrap();
        assert_eq!(value["result"], json!(["approved"]));
    }
    let value = call(&plugin, "callback").unwrap();
    assert!(value["error"].as_str().unwrap().contains("unsupported"));
}

#[test]
fn unload_and_close_release_session_lease_even_with_retained_adapter() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("worker");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join(installer::MANIFEST_FILE), b"fixture").unwrap();
    for close in [false, true] {
        let plugin = Arc::new(
            ProcessPlugin::start(
                "worker",
                Path::new("python3"),
                &["-u".into(), "-c".into(), DRIVER.into()],
                &BTreeMap::from([
                    ("DIAL_EXTENSION_RUNTIME".into(), "worker".into()),
                    ("DIAL_EXTENSION_ROOT".into(), root.display().to_string()),
                ]),
                2,
            )
            .unwrap(),
        );
        let host = Host::default();
        host.register(plugin.clone()).unwrap();
        assert!(lease::exclusive(dir.path(), "worker").is_err());
        if close {
            host.close();
        } else {
            assert!(host.unregister("worker"));
        }
        let _publication = lease::exclusive(dir.path(), "worker").unwrap();
        assert!(call(plugin.as_ref(), "count").is_err());
    }
}

#[test]
fn cancellation_discards_worker_until_explicit_restart() {
    let plugin = process("worker");
    let cancel = AtomicBool::new(false);
    let result = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(100));
            cancel.store(true, Ordering::Release);
        });
        plugin.call("wait", Value::Null, &cancel)
    });
    assert!(result.is_err());
    assert!(call(&plugin, "count").is_err());
    plugin.restart(&AtomicBool::new(false)).unwrap();
    assert_eq!(call(&plugin, "count").unwrap(), 1);
}

#[test]
fn callback_limit_discards_worker() {
    let plugin = process("worker");
    let mut count = 0;
    let error = plugin
        .call_with_host(
            "flood",
            Value::Null,
            &AtomicBool::new(false),
            &mut |_, _| {
                count += 1;
                Ok(Value::Null)
            },
        )
        .unwrap_err();
    assert!(error.to_string().contains("callback limit"));
    assert_eq!(count, 256);
    assert!(call(&plugin, "count").is_err());
}
