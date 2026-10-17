use super::*;

#[test]
fn command_requires_one_declared_activation() {
    assert!(command_index(std::iter::empty()).is_err());
    assert!(command_index(["show", "open", "focus"].into_iter()).is_err());
    assert!(command_index(["click", "activate"].into_iter()).is_err());
    assert!(command_index(["press", "PRESS"].into_iter()).is_err());
    assert_eq!(command_index(["show", "aCtIvAtE"].into_iter()).unwrap(), 1);
}

#[test]
fn non_menu_controls_are_not_commands() {
    assert!(!menu_role(Role::Button));
    assert!(!menu_role(Role::Frame));
    assert!(!menu_role(Role::MenuBar));
    assert!(menu_role(Role::CheckMenuItem));
    assert!(menu_role(Role::RadioMenuItem));
}
