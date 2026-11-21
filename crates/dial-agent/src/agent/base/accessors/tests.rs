use super::*;
use crate::agent::approval::{self, Effect, PermissionRule};
use crate::agent::run::tool_exposure::default_offer_tests::with_agent;

#[test]
fn failed_session_workspace_preparation_returns_error_without_swapping() {
    with_agent("", |agent| {
        let layout = agent.layout().unwrap().clone();
        let current = agent.session().id().to_owned();
        let checkpoints = agent.checkpoints().unwrap().clone();
        let background = agent.background_control().unwrap();
        agent.set_plan_mode(true);
        let next = Session::create(
            &layout,
            session::SessionMeta {
                model: "test".into(),
            },
        )
        .unwrap();
        std::fs::write(
            layout.checkpoints_dir().join(next.id()),
            "checkpoint storage is not a directory",
        )
        .unwrap();

        assert!(agent.swap_session(next).is_err());
        assert_eq!(agent.session().id(), current);
        assert!(agent.plan_mode());
        assert!(Arc::ptr_eq(&checkpoints, agent.checkpoints().unwrap()));
        assert!(Arc::ptr_eq(
            &background,
            &agent.background_control().unwrap()
        ));
    });
}

fn command_rule(effect: Effect, words: &[&str]) -> PermissionRule {
    PermissionRule {
        effect,
        tool: "bash".into(),
        command_prefix: words.iter().map(|word| (*word).to_owned()).collect(),
        paths: Vec::new(),
    }
}

fn saved_rules(agent: &Agent<'_>) -> Vec<PermissionRule> {
    let path = agent.layout().unwrap().state_dir.join("permissions.toml");
    approval::load(&path).unwrap()
}

#[test]
fn project_permission_edits_write_only_the_named_rule() {
    with_agent("", |agent| {
        let deny = command_rule(Effect::Deny, &["rm"]);
        let git = command_rule(Effect::Allow, &["git", "status"]);
        let session_only = command_rule(Effect::Allow, &["cargo", "test"]);
        let make = command_rule(Effect::Allow, &["make"]);

        agent.add_permission_rule(deny.clone(), true).unwrap();
        agent.add_permission_rule(git.clone(), true).unwrap();
        agent
            .add_permission_rule(session_only.clone(), false)
            .unwrap();
        assert_eq!(saved_rules(agent), vec![deny.clone(), git.clone()]);

        agent.add_permission_rule(make.clone(), true).unwrap();
        assert_eq!(
            saved_rules(agent),
            vec![deny.clone(), git.clone(), make.clone()],
            "saving one project rule must not promote session-only grants"
        );
        assert_eq!(
            agent.permission_rules(),
            &[
                deny.clone(),
                git.clone(),
                session_only.clone(),
                make.clone()
            ]
        );

        let session_index = agent
            .permission_rules()
            .iter()
            .position(|rule| *rule == git)
            .unwrap();
        agent.remove_permission_rule(session_index, true).unwrap();
        assert_eq!(saved_rules(agent), vec![deny.clone(), make.clone()]);
        assert_eq!(
            agent.permission_rules(),
            &[deny.clone(), session_only.clone(), make.clone()]
        );

        agent.remove_permission_rule(1, false).unwrap();
        assert_eq!(saved_rules(agent), vec![deny, make]);
    });
}

#[test]
fn failed_project_save_leaves_session_permissions_unchanged() {
    with_agent("", |agent| {
        let rule = command_rule(Effect::Allow, &["make"]);
        agent.add_permission_rule(rule.clone(), false).unwrap();
        let path = agent.layout().unwrap().state_dir.join("permissions.toml");
        // A directory where the rules file should be makes the atomic replace fail.
        std::fs::create_dir_all(&path).unwrap();

        assert!(
            agent
                .add_permission_rule(command_rule(Effect::Allow, &["cargo"]), true)
                .is_err()
        );
        assert!(agent.remove_permission_rule(0, true).is_err());
        assert_eq!(agent.permission_rules(), std::slice::from_ref(&rule));
    });
}

#[test]
fn approved_project_rule_persists_without_session_grants() {
    with_agent("", |agent| {
        let session_only = command_rule(Effect::Allow, &["cargo", "test"]);
        let approved = command_rule(Effect::Allow, &["make"]);
        agent
            .add_permission_rule(session_only.clone(), false)
            .unwrap();
        // The approval path appends its rule to the session list before asking for persistence.
        agent.always.push(approved.clone());

        agent.persist_latest_permission_rule().unwrap();
        assert_eq!(saved_rules(agent), vec![approved]);
        assert_eq!(agent.permission_rules().len(), 2);
        assert_eq!(agent.permission_rules()[0], session_only);
    });
}
