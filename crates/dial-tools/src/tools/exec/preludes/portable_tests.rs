use super::*;

#[test]
fn portable_api_ablation_removes_only_the_core_prelude() {
    let source = prelude_source_for_mode(PreludeLanguage::Js, PreludeMode::ExtensionsOnly);
    assert!(!source.contains("var dial ="));
    let source = prelude_source_for_mode(PreludeLanguage::Js, PreludeMode::Portable);
    assert!(source.contains("var dial ="));
    assert!(source.contains("var __dial_binary__"));
}

#[test]
fn caught_failed_checks_remain_in_native_receipts() {
    let display = DisplayBuffer::default();
    let mut dispatch = |_| panic!("a local check must not call tools");
    for passed in [true, false] {
        let reply = bridge_reply(
            &json!({"callback":"check","name":"known answer","passed":passed}),
            Some(EvalBackend::Node),
            None,
            &display,
            &mut dispatch,
        )
        .unwrap()
        .unwrap();
        assert_eq!(reply["is_error"], !passed);
    }
    assert_eq!(
        display.checks(),
        vec![
            json!({"name":"known answer","passed":true,"runtime":"node"}),
            json!({"name":"known answer","passed":false,"runtime":"node"})
        ]
    );
    assert!(
        bridge_reply(
            &json!({"callback":"check","name":"invalid","passed":"true"}),
            Some(EvalBackend::Node),
            None,
            &display,
            &mut dispatch
        )
        .unwrap()
        .is_err()
    );
    assert_eq!(display.checks().len(), 2);
}

#[test]
fn bound_checks_use_a_fresh_admitted_read_and_the_controller_backend() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let path = "example.bin";
    let bytes = [0xff, 0, 1];
    std::fs::write(dir.path().join(path), bytes).unwrap();
    let source = json!({"path": path, "sha256": crate::security::contracts::sha256_hex(&bytes)});
    let request = json!({"callback":"check","runtime":"spoofed","name":"bytes","passed":true,"source":source});
    let display = DisplayBuffer::default();
    let mut read = |callback| {
        let ToolCallback::Call { name, arguments } = callback else {
            panic!("expected tool dispatch")
        };
        assert_eq!(name, "read");
        assert_eq!(arguments, json!({"path":path,"format":"base64"}));
        crate::tools::fs::data_io::read(&ws, &arguments)
    };
    let check = |dispatch: &mut dyn FnMut(ToolCallback) -> Result<ToolOutput, String>| {
        bridge_reply(
            &request,
            Some(EvalBackend::QuickJs),
            Some(&ws),
            &display,
            dispatch,
        )
        .unwrap()
    };
    assert!(!check(&mut read).unwrap()["is_error"].as_bool().unwrap());
    // Neither denial nor an unconfirmed or stale read can attach a new pass.
    assert!(check(&mut |_| Err("blocked".into())).is_err());
    assert!(check(&mut |_| Ok(ToolOutput::structured(source.clone()))).is_err());
    assert!(
        check(&mut |cb| {
            let mut result = read(cb)?;
            result.data.as_mut().unwrap()["content"] = json!("AAAA");
            Ok(result)
        })
        .is_err()
    );
    std::fs::write(dir.path().join(path), [0]).unwrap();
    assert!(check(&mut read).is_err());
    assert_eq!(
        display.checks(),
        [json!({"name":"bytes","passed":true,"runtime":"quickjs","source":source})]
    );
}

#[test]
fn check_sources_must_be_portable_literal_workspace_paths() {
    use crate::tools::exec::eval::is_check_source_path;
    for path in [
        "",
        "../outside",
        "a/../b",
        "/tmp/file",
        "./a",
        "a//b",
        "artifact://ab",
        "C:\\file",
        "@f12",
        "@f1#AA",
        "~/private",
        "$TMPDIR",
        "${TMPDIR}/scratch",
        "x\ny",
    ] {
        assert!(!is_check_source_path(path), "{path:?}");
    }
    assert!(is_check_source_path("sub/a file.txt"));
}
