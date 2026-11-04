use super::*;
use dial::config::settings_store::{self, Change, Scope};

#[test]
fn installed_checkpoint_persists_without_reassigning_decision() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().join("dial")).unwrap();
    settings_store::save_config(
        &layout,
        Scope::Global,
        &[
            Change::Set {
                path: "model_roles.decision.provider".into(),
                value: "openai".into(),
            },
            Change::Set {
                path: "model_roles.decision.model".into(),
                value: "chosen-chat-model".into(),
            },
        ],
    )
    .unwrap();
    let checkpoint = home.path().join("installed-checkpoint");
    save_checkpoint(&layout, &checkpoint).unwrap();
    let saved = settings_store::load_config(&layout, &toml::Table::new()).unwrap();
    assert_eq!(saved.laya.checkpoint.as_deref(), checkpoint.to_str());
    let decision = saved.role_selection("decision").unwrap();
    assert_eq!(decision.provider, "openai");
    assert_eq!(decision.model, "chosen-chat-model");
}
