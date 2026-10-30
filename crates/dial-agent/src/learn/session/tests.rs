use super::*;
use crate::{config::types::Outcome, storage::session::SessionMeta};
use serde_json::json;

fn example(session: &Session) -> SessionTrainingExample {
    SessionTrainingExample {
        version: 1,
        source_session: session.id().into(),
        source_turn: "request-1".into(),
        source_model: "source-a".into(),
        source_state: None,
        source_trajectory: None,
        prompt: "original task".into(),
        payload: json!({"examples":[{"messages":[
            {"role":"user","content":"original task"},
            {"role":"assistant","content":"retained answer"}
        ],"tools":[]}]}),
    }
}

#[test]
fn switched_target_moves_only_default_datasets_without_mutating_saved_config() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().join("dial")).unwrap();
    let mut saved = Config::default().with_selection(&ModelSelection::new("mlx", "source-a"));
    saved.state_dir = saved.learning_dir(&layout);
    saved.training.train_data = saved.state_dir.join("grounded/train.jsonl");
    saved.training.valid_data = saved.state_dir.join("grounded/valid.jsonl");
    let selection = ModelSelection::new("mlx", "target-b");
    for custom_train in [false, true] {
        for custom_valid in [false, true] {
            let mut input = saved.clone();
            if custom_train {
                input.training.train_data = home.path().join("curated/train.jsonl");
            }
            if custom_valid {
                input.training.valid_data = home.path().join("curated/valid.jsonl");
            }
            let target = target_config(&layout, &input, &selection).unwrap();
            assert_eq!(target.model, "target-b");
            assert_ne!(target.state_dir, input.state_dir);
            assert_eq!(target.state_dir, target.learning_dir(&layout));
            assert_eq!(
                target.training.train_data,
                if custom_train {
                    input.training.train_data.clone()
                } else {
                    target.state_dir.join("grounded/train.jsonl")
                }
            );
            assert_eq!(
                target.training.valid_data,
                if custom_valid {
                    input.training.valid_data.clone()
                } else {
                    target.state_dir.join("grounded/valid.jsonl")
                }
            );
            assert_eq!(input.state_dir, saved.state_dir);
            assert_eq!(input.model, "source-a");
        }
    }
}

#[test]
fn source_snapshot_import_is_idempotent_after_reopen_and_fork_without_target_metrics() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().join("dial")).unwrap();
    let config = Config::default().with_selection(&ModelSelection::new("mlx", "target-b"));
    let config = target_config(&layout, &config, &ModelSelection::new("mlx", "target-b")).unwrap();
    let target = Store::open(&config.state_dir, "target-b", "case", &Default::default()).unwrap();
    let mut session = Session::create(
        &layout,
        SessionMeta {
            model: "source-a".into(),
        },
    )
    .unwrap();
    session
        .append(json!({"role":"user","content":"original task"}))
        .unwrap();
    let mut retained = example(&session);
    let source_path = config.state_dir.parent().unwrap().join("source-a");
    let source = Store::open(&source_path, "source-a", "case", &Default::default()).unwrap();
    let trajectory = source
        .begin_trajectory(
            source.active().unwrap().id,
            "coding",
            "task",
            &retained.prompt,
        )
        .unwrap();
    source
        .finish_trajectory(trajectory, Some("done"), None)
        .unwrap();
    retained.source_state = Some(source_path);
    retained.source_trajectory = Some(trajectory);
    session
        .append_custom_entry("training_turn", serde_json::to_value(&retained).unwrap())
        .unwrap();
    session
        .append(json!({"role":"assistant","content":"display transcript is not the snapshot"}))
        .unwrap();
    assert_eq!(approve(&config, &target, &session).unwrap().approved, 1);
    assert_eq!(approve(&config, &target, &session).unwrap().already, 1);
    let path = session.path().unwrap().to_owned();
    drop(session);
    drop(target);
    let target = Store::open(&config.state_dir, "target-b", "case", &Default::default()).unwrap();
    let session = Session::open(&path).unwrap();
    assert_eq!(approve(&config, &target, &session).unwrap().already, 1);
    let fork = session.fork(&layout).unwrap();
    assert_ne!(fork.id(), retained.source_session);
    assert_eq!(approve(&config, &target, &fork).unwrap().already, 1);
    let stored = target.session_training_examples().unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].1, retained);
    assert_eq!(target.verified_live_turns().unwrap(), 0);
    assert_eq!(
        target.live_score(target.active().unwrap().id).unwrap(),
        (0, None)
    );
}

#[test]
fn negative_source_verdict_is_not_overridden_by_target_approval() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().join("dial")).unwrap();
    let mut config = Config::default();
    config.state_dir = home.path().join("models/target-b");
    let target = Store::open(&config.state_dir, "target-b", "case", &Default::default()).unwrap();
    let source_path = home.path().join("models/source-a");
    let source = Store::open(&source_path, "source-a", "case", &Default::default()).unwrap();
    let trajectory = source
        .begin_trajectory(
            source.active().unwrap().id,
            "coding",
            "task",
            "original task",
        )
        .unwrap();
    source
        .finish_trajectory(trajectory, Some("done"), None)
        .unwrap();
    source
        .outcome(&Outcome {
            trajectory,
            score: 25.0,
            source: "user".into(),
            reference: None,
        })
        .unwrap();
    let mut session = Session::create(
        &layout,
        SessionMeta {
            model: "source-a".into(),
        },
    )
    .unwrap();
    let mut retained = example(&session);
    retained.source_state = Some(source_path);
    retained.source_trajectory = Some(trajectory);
    session
        .append_custom_entry("training_turn", serde_json::to_value(retained).unwrap())
        .unwrap();
    let report = approve(&config, &target, &session).unwrap();
    assert_eq!((report.approved, report.already, report.judged), (0, 0, 1));
    assert!(target.session_training_examples().unwrap().is_empty());
    assert_eq!(source.trajectory_score(trajectory).unwrap(), Some(25.0));
}
