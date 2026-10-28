use super::*;
use crate::app::core::App;

/// A download-backed plain setting (no model role) is written only when the download succeeds
/// and was not cancelled; failures and cancellations keep the previous value.
#[test]
fn downloaded_setting_saves_only_after_a_successful_uncancelled_download() {
    let (_home, _project, layout, config) = crate::app::tests::config();
    let app = App::new(
        layout,
        config,
        toml::Table::new(),
        Default::default(),
        vec![],
        Default::default(),
    )
    .unwrap();
    let repo = "mlx-community/draft-test";
    for (cancelled, success) in [(true, true), (false, false), (false, true)] {
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut job = Some(AuthJob {
            label: "Model",
            receiver,
            lines: None,
            cancel: Arc::new(AtomicBool::new(cancelled)),
            model: Some(PreparedModel {
                changes: vec![Change::Set {
                    path: "local_runtime.draft_model".into(),
                    value: toml::Value::String(repo.into()),
                }],
                role: None,
                scope: Scope::Global,
            }),
        });
        assert!(AuthJob::collect(&mut job, &app).is_none());
        assert_ne!(
            app.reload().unwrap().local_runtime.draft_model.as_deref(),
            Some(repo),
            "nothing is written while the download runs"
        );
        sender
            .send(if success {
                Ok("downloaded".into())
            } else {
                Err(anyhow::anyhow!("download failed"))
            })
            .unwrap();
        let (_, result) = AuthJob::collect(&mut job, &app).unwrap();
        assert_eq!(result.is_ok(), success && !cancelled);
        assert_eq!(
            app.reload().unwrap().local_runtime.draft_model.as_deref() == Some(repo),
            success && !cancelled
        );
    }
}
