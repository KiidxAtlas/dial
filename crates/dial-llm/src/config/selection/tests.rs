use super::*;

#[test]
fn typed_laya_selections_never_acquire_chat_weights() {
    let config = Config::default();
    for model in [
        super::super::laya::JUDGE_MODEL,
        "/tmp/laya-checkpoint",
        "owner/checkpoint",
    ] {
        assert!(!requires_local_preparation(
            &config,
            &ModelSelection::new("laya", model)
        ));
    }
    assert!(requires_local_preparation(
        &config,
        &ModelSelection::new("mlx", "owner/chat")
    ));
    let mut served = ModelSelection::new("custom", "chat");
    served.base_url = Some("http://localhost:1234/v1".into());
    assert!(!requires_local_preparation(&config, &served));
}
