use crate::agent::safety::approval::parallel_read;
use crate::config::types::Config;
use crate::tools::integrations::ext::{Concurrency, Registry};
use serde_json::json;

#[test]
fn registered_ast_search_is_declared_shared_and_runs_in_parallel_reads() {
    let registry = Registry::standard(&Config::default(), Vec::new());
    let args = json!({"pattern": "foo($A)"});
    let tool = registry
        .get("ast_search")
        .expect("ast_search is registered");
    assert_eq!(tool.concurrency(&args), Concurrency::Shared);
    assert!(parallel_read(&registry, "ast_search", &args));
}
