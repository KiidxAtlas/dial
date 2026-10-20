use super::*;
use crate::config::types::Config;
use crate::ui::status::StatusData;
use crate::ui::welcome::{Home, Pick, WelcomeInfo};

#[test]
fn idle_status_refresh_keeps_last_request_settings_in_plan_mode() {
    let config = Config {
        model: "model".into(),
        ..Default::default()
    };
    let mut status = StatusData::default();
    let mut home = None;
    update_model_status(&mut status, &mut home, &config);
    status.model.push_str(" [plan]");
    status.inference = "last request controls".into();
    update_model_status(&mut status, &mut home, &config);
    assert_eq!(status.inference, "last request controls");
}

#[test]
fn model_change_updates_status_and_home_without_resetting_navigation() {
    let mut status = StatusData::default();
    let mut home = Some(Home::new(
        WelcomeInfo {
            recent: vec![("existing session".into(), "1h".into())],
            ..Default::default()
        },
        vec!["session-id".into()],
        false,
    ));
    home.as_mut().unwrap().down();
    for model in ["first-model", "replacement-model"] {
        let config = Config {
            model: model.into(),
            ..Default::default()
        };
        status.inference = "previous model settings".into();
        update_model_status(&mut status, &mut home, &config);
        assert_eq!(status.model, model);
        assert!(
            status.inference.is_empty(),
            "A model switch clears old request settings"
        );
        let home = home.as_ref().unwrap();
        assert_eq!(home.info.model, status.model);
        assert_eq!(home.info.provider, status.provider);
        assert_eq!(home.selected(), Some(0));
    }
    assert_eq!(
        home.as_mut().unwrap().pick(),
        Some(Pick::Resume("session-id".into()))
    );
    home = None;
    update_model_status(&mut status, &mut home, &Config::default());
    assert!(
        home.is_none(),
        "A conversation must not reopen the welcome screen"
    );
}
