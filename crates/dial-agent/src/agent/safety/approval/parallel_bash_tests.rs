use super::*;
use serde_json::json;

#[test]
fn only_stateless_read_only_bash_joins_parallel_groups() {
    let ext = Registry::new();
    for args in [
        json!({"command":"ls -la"}),
        json!({"command":"cat README.md","timeout":30}),
        json!({"command":"ls","background":false}),
    ] {
        assert!(parallel_read(&ext, "bash", &args), "{args}");
    }
    for args in [
        json!({"command":"rm -rf target"}),
        json!({"command":"ls > out.txt"}),
        json!({"command":"ls","background":true}),
        json!({"command":"ls","async":true}),
        json!({"command":"ls","session":"review"}),
        json!({"command":"ls","background":"yes"}),
    ] {
        assert!(!parallel_read(&ext, "bash", &args), "{args}");
    }
}
