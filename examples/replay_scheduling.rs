//! Analyze recorded calls without replaying their effects or contacting a model.
fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let workspace = args.next().ok_or_else(|| {
        anyhow::anyhow!("usage: replay_scheduling WORKSPACE SESSION.jsonl [SESSION.jsonl ...]")
    })?;
    let config = dial::config::types::Config::default();
    let workspace = dial::tools::Workspace::new(std::path::Path::new(&workspace))?;
    let registry = dial::tools::integrations::ext::Registry::standard(&config, Vec::new());
    let mut responses = Vec::new();
    let mut sessions = 0;
    for path in args {
        let session = dial::storage::session::Session::read(std::path::Path::new(&path))?;
        sessions += 1;
        for message in session.archived_messages() {
            if message["role"] != "assistant" || !message["tool_calls"].is_array() {
                continue;
            }
            let reply = dial::llm::parse_reply(&serde_json::to_vec(&serde_json::json!({
                "choices":[{"message":message,"finish_reason":"tool_calls"}]
            }))?)?;
            responses.push(reply.tool_calls);
        }
    }
    anyhow::ensure!(sessions > 0, "supply at least one session file");
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "sessions": sessions,
            "basis": "recorded call order; current workspace metadata; default config; no tools executed",
            "limitations": "structural opportunities only; historical files, live policy, preflights and streaming timing unavailable; no wall-time prediction",
            "comparison": dial::agent::replay_scheduling(&workspace,&config,&registry,&responses)
        }))?
    );
    Ok(())
}
