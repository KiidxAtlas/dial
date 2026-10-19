use crate::config::{paths::Layout, types::Config};
use std::fs;

use super::test_support::{FakeRunner, authority};
use super::*;

fn semantic_runtime(
    dir: &std::path::Path,
    memory_role: bool,
) -> (Arc<MemoryRuntime>, MemoryAuthority) {
    let project = dir.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let layout = Layout::with_user_dir(&project, dir.join("home")).unwrap();
    let mut config = MemoryConfig {
        backend: MemoryBackendId::Semantic,
        ..MemoryConfig::default()
    };
    config.semantic.embedding = crate::config::memory_backend::EmbeddingMode::Off;
    let scope = bank_scope(&layout, &config);
    let authority = authority("session-a");
    let runtime = MemoryRuntime::with_scope(
        &layout,
        &config,
        authority.clone(),
        scope,
        None,
        Ok(None),
        memory_role,
    )
    .unwrap();
    assert!(runtime.open_error().is_none(), "{:?}", runtime.open_error());
    (runtime, authority)
}

fn pair(prompt: &str, answer: &str) -> [Value; 2] {
    [
        json!({"role":"user","content":prompt}),
        json!({"role":"assistant","content":answer}),
    ]
}

#[test]
fn fact_refs_parse_both_banks_and_reject_garbage() {
    assert_eq!(
        FactRef::parse("12").unwrap(),
        FactRef {
            global: false,
            id: 12
        }
    );
    assert_eq!(
        FactRef::parse("user:3").unwrap(),
        FactRef {
            global: true,
            id: 3
        }
    );
    assert_eq!(FactRef::parse("user:3").unwrap().render(), "user:3");
    for bad in ["", "user:", "x", "0", "-1", "user:abc"] {
        assert!(FactRef::parse(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn every_user_message_recalls_only_facts_not_already_shown() {
    let dir = tempfile::tempdir().unwrap();
    let (runtime, auth) = semantic_runtime(dir.path(), true);
    let mut runner = FakeRunner::new("{}");
    runtime
        .command(
            &auth,
            "save the deploy vault token rotates monthly",
            &mut runner,
        )
        .unwrap();
    let first = runtime
        .prepare_prompt(&auth, "how does the deploy vault work?", &mut runner)
        .unwrap()
        .unwrap();
    let context = first.context.clone().unwrap();
    assert!(context.contains("vault token rotates"), "{context}");
    assert!(first.commit(&auth));
    runtime
        .command(
            &auth,
            "save the deploy pipeline uses blue green slots",
            &mut runner,
        )
        .unwrap();
    // A later message is queried again; only the fact not shown yet is attached.
    let second = runtime
        .prepare_prompt(&auth, "remind me about the deploy setup", &mut runner)
        .unwrap()
        .unwrap();
    let context = second.context.clone().unwrap();
    assert!(context.contains("blue green"), "{context}");
    assert!(!context.contains("vault token"), "{context}");
    assert!(second.commit(&auth));
    // Nothing new is relevant: no block is attached, and nothing cached is re-appended.
    let third = runtime
        .prepare_prompt(&auth, "anything else about deploy?", &mut runner)
        .unwrap()
        .unwrap();
    assert!(third.context.is_none());
    assert!(third.commit(&auth));
    // The identical message is not re-queried.
    assert!(
        runtime
            .prepare_prompt(&auth, "anything else about deploy?", &mut runner)
            .unwrap()
            .is_none()
    );
    // Transient (child) context carries every fact shown so far.
    let shown = runtime.prompt_context(&auth).unwrap().unwrap();
    assert!(shown.contains("vault token") && shown.contains("blue green"));
    // A new session branch starts with nothing shown.
    let other = authority("session-b");
    runtime.bind(other.clone());
    let fresh = runtime
        .prepare_prompt(&other, "how does the deploy vault work?", &mut runner)
        .unwrap()
        .unwrap();
    assert!(fresh.context.unwrap().contains("vault token"));
}

#[test]
fn clear_resets_shown_facts_and_keeps_the_user_bank() {
    let dir = tempfile::tempdir().unwrap();
    let (runtime, auth) = semantic_runtime(dir.path(), true);
    let mut runner = FakeRunner::new("{}");
    let saved = runtime
        .execute_tool(
            &auth,
            "retain",
            &json!({"items":[
                {"content":"the build cache lives in target dir"},
                {"content":"the user prefers terse build answers","scope":"global"}
            ]}),
            &mut runner,
        )
        .unwrap();
    assert_eq!(saved["results"][0]["ref"], "1");
    assert_eq!(saved["results"][1]["ref"], "user:1");
    let recalled = runtime
        .execute_tool(&auth, "recall", &json!({"query":"build"}), &mut runner)
        .unwrap();
    let refs: Vec<&str> = recalled["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["ref"].as_str().unwrap())
        .collect();
    assert_eq!(refs, vec!["1", "user:1"]);
    let prepared = runtime
        .prepare_prompt(&auth, "where is the build cache?", &mut runner)
        .unwrap()
        .unwrap();
    assert!(prepared.context.is_some());
    assert!(prepared.commit(&auth));
    let cleared = runtime.command(&auth, "clear", &mut runner).unwrap();
    assert_eq!(cleared["bank"], "project");
    assert_eq!(cleared["forgotten"], 1);
    let status = runtime.command(&auth, "status", &mut runner).unwrap();
    assert_eq!(status["facts"], 0);
    assert_eq!(status["userFacts"], 1);
    // Clearing reset the shown set: the surviving user-wide fact is attached again.
    let again = runtime
        .prepare_prompt(&auth, "where is the build cache?", &mut runner)
        .unwrap()
        .unwrap();
    assert!(again.context.unwrap().contains("terse build answers"));
    let cleared = runtime.command(&auth, "clear user", &mut runner).unwrap();
    assert_eq!(cleared["bank"], "user");
    let status = runtime.command(&auth, "status", &mut runner).unwrap();
    assert_eq!(status["userFacts"], 0);
    assert!(
        runtime
            .command(&auth, "clear everything", &mut runner)
            .is_err()
    );
}

#[test]
fn bank_qualified_refs_target_the_right_bank() {
    let dir = tempfile::tempdir().unwrap();
    let (runtime, auth) = semantic_runtime(dir.path(), true);
    let mut runner = FakeRunner::new("{}");
    runtime
        .execute_tool(
            &auth,
            "retain",
            &json!({"items":[
                {"content":"project fact about the parser"},
                {"content":"user fact about the parser","scope":"global"}
            ]}),
            &mut runner,
        )
        .unwrap();
    // Both banks number from 1; `user:1` must never touch project fact 1.
    runtime
        .command(&auth, "forget user:1 1", &mut runner)
        .unwrap();
    let status = runtime.command(&auth, "status", &mut runner).unwrap();
    assert_eq!(
        (status["facts"].clone(), status["userFacts"].clone()),
        (json!(1), json!(0))
    );
    let history = runtime
        .command(&auth, "history user:1", &mut runner)
        .unwrap();
    assert_eq!(history["history"][0]["action"], "forget");
    let updated = runtime
        .command(
            &auth,
            "update 1 1 project fact about the lexer",
            &mut runner,
        )
        .unwrap();
    assert_eq!(updated["revision"], 2);
    // The model's correction tools take provenance's id, scope and revision arguments.
    let names = runtime.offered_names();
    assert!(names.contains(&"memory_update") && names.contains(&"forget"));
    let stale = runtime.execute_tool(
        &auth,
        "forget",
        &json!({"id":1,"scope":"project","expected_revision":1}),
        &mut runner,
    );
    assert!(stale.is_err(), "a stale revision must not forget");
    runtime
        .execute_tool(
            &auth,
            "memory_update",
            &json!({"id":1,"scope":"project","expected_revision":2,"content":"project fact about the tokenizer"}),
            &mut runner,
        )
        .unwrap();
    runtime
        .execute_tool(
            &auth,
            "forget",
            &json!({"id":1,"scope":"project","expected_revision":3}),
            &mut runner,
        )
        .unwrap();
    let status = runtime.command(&auth, "status", &mut runner).unwrap();
    assert_eq!(status["facts"], 0);
    let help = runtime.command(&auth, "help", &mut runner).unwrap();
    let help = help["help"].to_string();
    assert!(
        help.contains("user:<n>") && help.contains("/memory clear user"),
        "{help}"
    );
}

#[test]
fn auto_retain_pauses_without_a_memory_role_but_recall_and_retain_work() {
    let dir = tempfile::tempdir().unwrap();
    let (runtime, auth) = semantic_runtime(dir.path(), false);
    let mut runner = FakeRunner::new(r#"{"facts":[{"content":"never stored","scope":"project"}]}"#);
    let messages: Vec<Value> = pair(
        "please remember that our staging database is called orchid",
        "Noted: staging is orchid.",
    )
    .into();
    runtime.observe_owned(&auth, messages).unwrap();
    runtime.maintain(&auth, &mut runner).unwrap();
    assert!(
        runner.systems.is_empty(),
        "no model call without the memory role"
    );
    let status = runtime.command(&auth, "status", &mut runner).unwrap();
    assert_eq!(status["autoRetain"], "paused");
    assert!(
        status["notes"]
            .to_string()
            .contains("assign the memory role (Alt+M) to save facts automatically"),
        "{status}"
    );
    runtime
        .execute_tool(
            &auth,
            "retain",
            &json!({"items":[{"content":"staging database is orchid"}]}),
            &mut runner,
        )
        .unwrap();
    let recalled = runtime
        .execute_tool(&auth, "recall", &json!({"query":"staging"}), &mut runner)
        .unwrap();
    assert_eq!(recalled["items"].as_array().unwrap().len(), 1);
}

#[test]
fn backlog_keeps_every_turn_across_turns_and_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let (runtime, auth) = semantic_runtime(dir.path(), true);
    let mut runner = FakeRunner::new(r#"{"facts":[]}"#);
    let mut first: Vec<Value> = pair(
        "the first long question about the release checklist",
        "First answer.",
    )
    .into();
    runtime.observe_owned(&auth, first.clone()).unwrap();
    // Maintenance did not run; a second turn finishes on the same branch.
    first.extend(pair(
        "the second long question about the release checklist",
        "Second answer.",
    ));
    runtime.observe_owned(&auth, first).unwrap();
    // The user switches session before maintenance; that turn is not lost either.
    let other = authority("session-b");
    runtime.bind(other.clone());
    let second: Vec<Value> = pair(
        "a third long question in another session entirely",
        "Third answer.",
    )
    .into();
    runtime.observe_owned(&other, second.clone()).unwrap();
    runtime.maintain(&other, &mut runner).unwrap();
    assert_eq!(runner.systems.len(), 3, "every turn is extracted once");
    // Re-observing the same archive queues nothing new.
    runtime.observe_owned(&other, second).unwrap();
    runtime.maintain(&other, &mut runner).unwrap();
    assert_eq!(runner.systems.len(), 3);
}

#[test]
fn memory_setup_can_load_and_open_before_role_assignment() {
    use crate::storage::memory_backend::{MemoryAuthority, MemoryRuntime};
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_owned()).unwrap();
    let path = layout.config_files()[0].clone();
    let settings = "model='local'\nprovider='mlx'\n[memory]\nbackend='local'\n";
    fs::write(&path, settings).unwrap();
    let authority = MemoryAuthority {
        session_id: "setup-session".into(),
        branch_id: "main".into(),
        revision: "0".into(),
        task_depth: 0,
    };
    let config = Config::load(&layout, None).unwrap();
    let runtime = MemoryRuntime::open_configured(&layout, &config, authority.clone()).unwrap();
    assert_eq!(runtime.id(), config.memory.backend);
    assert!(
        runtime
            .open_error()
            .unwrap()
            .contains("assign the memory role")
    );
    let child = runtime
        .for_child(MemoryAuthority {
            task_depth: 1,
            ..authority.clone()
        })
        .unwrap();
    assert_eq!(child.open_error(), runtime.open_error());
    fs::write(
        &path,
        format!("{settings}[model_roles.memory]\nprovider='mlx'\nmodel='m'\n"),
    )
    .unwrap();
    let config = Config::load(&layout, None).unwrap();
    let runtime = MemoryRuntime::open_configured(&layout, &config, authority).unwrap();
    assert!(runtime.open_error().is_none(), "{:?}", runtime.open_error());
}

fn turn(role: &str, text: &str) -> Value {
    json!({"role": role, "content": text})
}

#[test]
fn identical_raw_prompt_requeries_only_when_its_effective_context_changes() {
    let dir = tempfile::tempdir().unwrap();
    let (runtime, auth) = semantic_runtime(dir.path(), true);
    let mut runner = FakeRunner::new("{}");
    runtime
        .command(
            &auth,
            "save the deploy vault token rotates monthly",
            &mut runner,
        )
        .unwrap();
    runtime
        .observe_owned(
            &auth,
            vec![
                turn("user", "earlier question"),
                turn("assistant", "earlier answer"),
            ],
        )
        .unwrap();
    let first = runtime
        .prepare_prompt(&auth, "continue", &mut runner)
        .unwrap()
        .unwrap();
    assert!(first.commit(&auth));
    // Same raw words against the same effective query: nothing new to find.
    assert!(
        runtime
            .prepare_prompt(&auth, "continue", &mut runner)
            .unwrap()
            .is_none()
    );
    // Same raw words after the conversation moved on are a different question and are queried.
    runtime
        .observe_owned(
            &auth,
            vec![
                turn("user", "earlier question"),
                turn("assistant", "earlier answer"),
                turn("user", "deploy vault?"),
                turn("assistant", "it rotates monthly"),
            ],
        )
        .unwrap();
    assert!(
        runtime
            .prepare_prompt(&auth, "continue", &mut runner)
            .unwrap()
            .is_some()
    );
}

#[test]
fn compaction_rebinds_the_history_generation_and_reattaches_compacted_away_facts() {
    let dir = tempfile::tempdir().unwrap();
    let (runtime, auth) = semantic_runtime(dir.path(), true);
    let mut runner = FakeRunner::new("{}");
    runtime
        .command(
            &auth,
            "save the deploy vault token rotates monthly",
            &mut runner,
        )
        .unwrap();
    let question = "how does the deploy vault work?";
    let first = runtime
        .prepare_prompt(&auth, question, &mut runner)
        .unwrap()
        .unwrap();
    assert!(first.context.as_deref().unwrap().contains("vault token"));
    assert!(first.commit(&auth));
    // While the fact is still in visible context, the identical question attaches nothing again.
    assert!(
        runtime
            .prepare_prompt(&auth, question, &mut runner)
            .unwrap()
            .is_none()
    );
    // Compaction advances the provider history generation, which memory_authority embeds in the
    // branch id. Rebinding to that branch resets the shown set, so the fact is attached again.
    let compacted = MemoryAuthority {
        branch_id: format!("{}:history:1", auth.session_id),
        ..auth.clone()
    };
    runtime.bind(compacted.clone());
    let again = runtime
        .prepare_prompt(&compacted, question, &mut runner)
        .unwrap()
        .unwrap();
    assert!(again.context.as_deref().unwrap().contains("vault token"));
    assert!(again.commit(&compacted));
    // The pre-compaction authority is stale and cannot publish or query into the new generation.
    assert!(
        runtime
            .prepare_prompt(&auth, question, &mut runner)
            .is_err()
    );
}

#[test]
fn rewind_rebinds_the_history_generation_and_reattaches_removed_facts() {
    let dir = tempfile::tempdir().unwrap();
    let (runtime, auth) = semantic_runtime(dir.path(), true);
    let mut runner = FakeRunner::new("{}");
    runtime
        .command(
            &auth,
            "save the deploy vault token rotates monthly",
            &mut runner,
        )
        .unwrap();
    let question = "how does the deploy vault work?";
    let first = runtime
        .prepare_prompt(&auth, question, &mut runner)
        .unwrap()
        .unwrap();
    assert!(first.context.as_deref().unwrap().contains("vault token"));
    assert!(first.commit(&auth));
    // While the fact is still in visible context, the identical question attaches nothing again.
    assert!(
        runtime
            .prepare_prompt(&auth, question, &mut runner)
            .unwrap()
            .is_none()
    );
    // A real rewind mints history generation 2 (see `session.rewind`); the removed turn no longer
    // shows its fact, so rebinding to the new branch must attach it again.
    let rewound = MemoryAuthority {
        branch_id: format!("{}:history:2", auth.session_id),
        ..auth.clone()
    };
    runtime.bind(rewound.clone());
    let again = runtime
        .prepare_prompt(&rewound, question, &mut runner)
        .unwrap()
        .unwrap();
    assert!(again.context.as_deref().unwrap().contains("vault token"));
    assert!(again.commit(&rewound));
    // The pre-rewind authority is stale and cannot publish or query into the rewound generation.
    assert!(
        runtime
            .prepare_prompt(&auth, question, &mut runner)
            .is_err()
    );
}
