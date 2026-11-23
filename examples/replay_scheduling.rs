//! Analyze recorded calls without replaying their effects or contacting a model.
fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let workspace = args.next().ok_or_else(|| {
        anyhow::anyhow!("usage: replay_scheduling WORKSPACE SESSION.jsonl [SESSION.jsonl ...]")
    })?;
    let config = dial::config::types::Config::default();
    let mut workspace = dial::tools::Workspace::new(std::path::Path::new(&workspace))?;
    let registry = dial::tools::integrations::ext::Registry::standard(&config, Vec::new());
    let mut comparison = dial::agent::SchedulingReplay::default();
    let mut sessions = 0;
    let mut handles_unrestored = 0;
    for path in args {
        let session = dial::storage::session::Session::read(std::path::Path::new(&path))?;
        sessions += 1;
        // Recorded calls may name files by session-local `@fN` handles. Restore this session's
        // mappings so they expand as they did live; without them they count as `file_reference`.
        workspace.reset_file_handles();
        let restored = session
            .control_state()
            .get("file_handles")
            .and_then(|snapshot| {
                serde_json::from_value::<dial::tools::file_handles::FileHandleSnapshot>(
                    snapshot.clone(),
                )
                .ok()
            })
            .is_some_and(|snapshot| workspace.restore_file_handles(&snapshot).is_ok());
        handles_unrestored += usize::from(!restored);
        let mut responses = Vec::new();
        for message in session.archived_messages() {
            if message["role"] != "assistant" || !message["tool_calls"].is_array() {
                continue;
            }
            let reply = dial::llm::parse_reply(&serde_json::to_vec(&serde_json::json!({
                "choices":[{"message":message,"finish_reason":"tool_calls"}]
            }))?)?;
            responses.push(reply.tool_calls);
        }
        comparison.extend(dial::agent::replay_scheduling(
            &workspace, &config, &registry, &responses,
        ));
    }
    anyhow::ensure!(sessions > 0, "supply at least one session file");
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "sessions": sessions,
            "sessions_without_file_handles": handles_unrestored,
            "basis": "recorded call order; current workspace metadata; recorded file handles; default config; no tools executed",
            "limitations": "structural opportunities only; historical files, live policy, preflights and streaming timing unavailable; no wall-time prediction",
            "comparison": comparison
        }))?
    );
    Ok(())
}
