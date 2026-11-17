use super::*;

#[test]
fn compact_scope_caption_names_the_actual_destination() {
    for width in [24, 36, 46] {
        let global = scope_caption(Scope::Global, "atlas", width);
        assert!(global.contains("All projects"), "{global}");
        assert!(!global.contains("atlas"), "{global}");
        let project = scope_caption(Scope::Project, "atlas", width);
        assert!(project.contains("This project"), "{project}");
        assert!(text::width(&global) <= width);
        assert!(text::width(&project) <= width);
    }
}

#[test]
fn scope_caption_keeps_switch_hint_when_it_fits() {
    for scope in [Scope::Global, Scope::Project] {
        let caption = scope_caption(scope, "atlas", 100);
        assert!(caption.contains("Ctrl+B"), "{caption}");
    }
}
