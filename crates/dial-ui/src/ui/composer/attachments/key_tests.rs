use super::*;
use crate::llm::model_config::UiConfig;
use crate::ui::{keys::Keymap, status::StatusData, theme::Theme};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

#[test]
fn held_enter_cannot_confirm_material_attachment_issues() {
    let theme = Theme::dark();
    let keys = Keymap::defaults();
    let ui = UiConfig::default();
    let status = StatusData::default();
    let ctx = super::super::ComposerCtx {
        theme: &theme,
        keys: &keys,
        ui: &ui,
        status: &status,
    };
    let mut composer = Composer::new(Vec::new(), 10);
    composer.set_draft("explain @missing");
    composer.refresh_attachments();
    composer.attachments.result = Some(Preview {
        issues: vec!["missing".into()],
        ..Preview::default()
    });
    let press = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        composer.handle_key(press, &ctx),
        super::super::ComposerEvent::None
    );
    assert!(composer.attachments.armed);
    let mut repeat = press;
    repeat.kind = KeyEventKind::Repeat;
    assert_eq!(
        composer.handle_key(repeat, &ctx),
        super::super::ComposerEvent::None
    );
    assert_eq!(composer.draft(), "explain @missing");
    assert_eq!(
        composer.handle_key(press, &ctx),
        super::super::ComposerEvent::Submit("explain @missing".into())
    );
}
