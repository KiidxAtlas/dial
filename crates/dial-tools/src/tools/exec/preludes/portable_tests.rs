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
            json!({"name":"known answer","passed":true}),
            json!({"name":"known answer","passed":false})
        ]
    );
    assert!(
        bridge_reply(
            &json!({"callback":"check","name":"invalid","passed":"true"}),
            &display,
            &mut dispatch
        )
        .unwrap()
        .is_err()
    );
    assert_eq!(display.checks().len(), 2);
}
