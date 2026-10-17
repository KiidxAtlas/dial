use super::*;
use serde_json::json;
#[test]
fn proposal_source_has_file_language_and_bounded_coverage() {
    let conditions = Conditions::compile(&["danger($A)".into()], Some("rust")).unwrap();
    assert!(
        conditions
            .matches(&json!({"path":"a.rs","content":"fn main() { danger(x); }"}))
            .unwrap()
    );
    assert!(
        !conditions
            .matches(&json!({"path":"a.rs","command":"danger(x)"}))
            .unwrap()
    );
    assert!(
        conditions
            .matches(&json!({"path":"a.rs","content":"x".repeat(129*1024)}))
            .is_err()
    );
}
