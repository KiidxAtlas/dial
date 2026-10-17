use super::*;

#[test]
fn absent_mode_keeps_the_default_chat_path() {
    assert_eq!(resolve(None, false).unwrap(), None);
}

#[test]
fn json_mode_is_the_event_stream_on_the_chat_path() {
    let r = resolve(Some("json"), false).unwrap().unwrap();
    assert_eq!(r.route, Route::Chat);
    assert_eq!(r.output, OutputFormat::StreamJson);
}

#[test]
fn protocol_modes_route_to_their_servers() {
    assert_eq!(
        resolve(Some("rpc"), false).unwrap().unwrap().route,
        Route::Rpc
    );
    assert_eq!(
        resolve(Some("rpc-ui"), false).unwrap().unwrap().route,
        Route::Rpc
    );
    assert_eq!(
        resolve(Some("acp"), false).unwrap().unwrap().route,
        Route::Acp
    );
}

#[test]
fn unknown_mode_is_rejected_with_the_valid_set() {
    let error = resolve(Some("gui"), false).unwrap_err();
    assert!(error.contains("text, json, rpc, acp, rpc-ui"), "{error}");
}

#[test]
fn no_ui_requires_an_rpc_mode() {
    assert!(resolve(None, true).is_err());
    assert!(resolve(Some("json"), true).is_err());
    assert!(resolve(Some("rpc"), true).unwrap().is_some());
    assert!(resolve(Some("rpc-ui"), true).unwrap().is_some());
}

#[test]
fn approval_aliases_map_onto_dial_policies() {
    assert_eq!(approval_alias("always-ask").unwrap(), "ask");
    assert_eq!(approval_alias("yolo").unwrap(), "auto");
    assert_eq!(approval_alias("write").unwrap(), "write");
    assert!(approval_alias("nope").is_err());
    for &(_, policy) in APPROVAL_ALIASES {
        assert!(crate::config::types::Approval::NAMES.contains(&policy));
    }
    for &policy in crate::config::types::Approval::NAMES {
        let parsed: crate::config::types::Approval =
            serde_json::from_value(serde_json::json!(policy)).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), policy);
        assert!(!parsed.description().is_empty());
    }
}
