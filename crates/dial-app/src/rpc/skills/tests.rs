use super::*;
use crate::config::paths::Layout;

fn code_of(error: anyhow::Error) -> ErrorCode {
    error
        .downcast_ref::<super::super::RequestError>()
        .map_or(ErrorCode::Failed, |refused| refused.code)
}

#[test]
fn skill_changes_map_to_owner_words_and_confirmation() {
    let accepted: Vec<(Value, Vec<&str>, bool)> = vec![
        (
            json!({"action":"enable","name":"review"}),
            vec!["enable", "review"],
            false,
        ),
        (
            json!({"action":"enable","name":"review","scope":"user"}),
            vec!["enable", "review", "--user"],
            false,
        ),
        (
            json!({"action":"disable","name":"review"}),
            vec!["disable", "review"],
            true,
        ),
        (
            json!({"action":"remove","name":"review"}),
            vec!["remove", "review"],
            true,
        ),
        (
            json!({"action":"install","spec":"acme/review"}),
            vec!["install", "acme/review"],
            true,
        ),
        (json!({"action":"update"}), vec!["update"], true),
        (
            json!({"action":"update","name":"review"}),
            vec!["update", "review"],
            true,
        ),
        (
            json!({"action":"import","path":"~/skills"}),
            vec!["import", "~/skills"],
            true,
        ),
        (
            json!({"action":"import","path":"~/skills","dry_run":true,"update":true}),
            vec!["import", "~/skills", "--dry-run", "--update"],
            false,
        ),
    ];
    for (params, words, needs_confirm) in accepted {
        let change = change_request(&params).unwrap_or_else(|error| panic!("{params}: {error:#}"));
        assert_eq!(change.words, words, "{params}");
        assert_eq!(change.needs_confirm, needs_confirm, "{params}");
    }
}

#[test]
fn malformed_skill_changes_are_refused_as_invalid_params() {
    let refused = [
        json!({"action":"enable"}),
        json!({"action":"enable","name":"-rf"}),
        json!({"action":"enable","name":"two words"}),
        json!({"action":"install","spec":""}),
        json!({"action":"import","path":"-rf"}),
        json!({"action":"rename","name":"review"}),
        json!({"action":"enable","name":"review","scope":"global"}),
        json!({"action":"enable","name":"review","shell":"sh -c id"}),
        json!({"name":"review"}),
    ];
    for params in refused {
        let error = change_request(&params).unwrap_err();
        assert_eq!(code_of(error), ErrorCode::InvalidParams, "{params}");
    }
}

#[test]
fn list_forms_are_read_only_owner_words() {
    let accepted: Vec<(Value, Vec<&str>)> = vec![
        (json!({}), vec!["list"]),
        (json!({"action":"sources"}), vec!["sources"]),
        (json!({"action":"installed"}), vec!["installed"]),
        (
            json!({"action":"search","query":"review  tool"}),
            vec!["search", "review", "tool"],
        ),
    ];
    for (params, words) in accepted {
        assert_eq!(list_words(&params).unwrap(), words, "{params}");
    }
    for params in [
        json!({"action":"install"}),
        json!({"action":"search"}),
        json!({"action":"search","query":"--user"}),
        json!({"action":"search","query":"a b c d e f g h i"}),
        json!({"action":"search","query":"x".repeat(201)}),
        json!({"action":7}),
    ] {
        assert_eq!(
            code_of(list_words(&params).unwrap_err()),
            ErrorCode::InvalidParams,
            "{params}"
        );
    }
}

#[test]
fn list_skills_answers_the_owner_text_for_an_empty_layout() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
    let result = list(&layout, &BTreeMap::new(), &json!({})).unwrap();
    assert_eq!(result["action"], "list");
    assert!(result["text"].as_str().is_some_and(|text| !text.is_empty()));
}

#[test]
fn changes_that_write_download_or_remove_need_confirm() {
    let cases = [
        (json!({"action":"enable","name":"review"}), false),
        (json!({"action":"disable","name":"review"}), true),
        (
            json!({"action":"import","path":"~/skills","dry_run":true}),
            false,
        ),
        (json!({"action":"import","path":"~/skills"}), true),
        (json!({"action":"install","spec":"acme/review"}), true),
        (json!({"action":"remove","name":"review"}), true),
    ];
    for (params, gated) in cases {
        let change = change_request(&params).unwrap();
        let refused = confirm_gate(&change, &params).err();
        assert_eq!(refused.is_some(), gated, "{params}");
        if let Some(error) = refused {
            assert_eq!(code_of(error), ErrorCode::InvalidParams, "{params}");
        }
        let confirmed = json!({"action": params["action"], "name": params["name"], "spec": params["spec"], "path": params["path"], "dry_run": params["dry_run"], "confirm": true});
        assert!(confirm_gate(&change_request(&confirmed).unwrap(), &confirmed).is_ok());
    }
}

fn sender() -> Sender {
    Sender {
        output: Arc::new(std::sync::Mutex::new(super::super::Output {
            acp: None,
            protocol: dial_protocol::DEFAULT_PROTOCOL_VERSION,
            interactive: false,
            envelope: super::super::Envelope::default(),
        })),
        writer: crate::output::StdoutWriter::new().unwrap(),
        failed: Arc::new(AtomicBool::new(false)),
        failure: Arc::new(std::sync::OnceLock::new()),
    }
}

/// Empty settings roots for a change request; admission never touches them.
fn layout(project: &tempfile::TempDir, home: &tempfile::TempDir) -> Layout {
    Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap()
}

#[test]
fn a_second_skill_change_is_refused_busy_while_one_runs() {
    let (project, home) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (jobs, out, layout) = (
        Arc::new(SkillJobs::default()),
        sender(),
        layout(&project, &home),
    );
    // A running worker holds the slot; a second change that needs no confirm is refused busy.
    jobs.running.store(true, Ordering::SeqCst);
    let enable = json!({"action":"enable","name":"review"});
    let error = start(&jobs, &out, &layout, BTreeMap::new(), &json!(1), &enable).unwrap_err();
    assert_eq!(code_of(error), ErrorCode::Busy);
    // An unconfirmed gated change is refused for the missing confirm before admission, so it
    // never reports busy and never takes the slot.
    let gated = json!({"action":"remove","name":"review"});
    let error = start(&jobs, &out, &layout, BTreeMap::new(), &json!(2), &gated).unwrap_err();
    assert_eq!(code_of(error), ErrorCode::InvalidParams);
    assert!(
        jobs.running.load(Ordering::SeqCst),
        "the running change keeps the slot"
    );
}

#[test]
fn a_refused_change_leaves_the_slot_free_and_the_worker_guard_releases_it() {
    let (project, home) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (jobs, out, layout) = (
        Arc::new(SkillJobs::default()),
        sender(),
        layout(&project, &home),
    );
    let gated = json!({"action":"install","spec":"acme/review"});
    let error = start(&jobs, &out, &layout, BTreeMap::new(), &json!(1), &gated).unwrap_err();
    assert_eq!(code_of(error), ErrorCode::InvalidParams);
    assert!(!jobs.running.load(Ordering::SeqCst));
    // The worker holds `Running`; dropping it, including by panic, clears the flag.
    jobs.running.store(true, Ordering::SeqCst);
    drop(Running(jobs.clone()));
    assert!(!jobs.running.load(Ordering::SeqCst));
}
