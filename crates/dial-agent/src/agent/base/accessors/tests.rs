use super::*;
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
