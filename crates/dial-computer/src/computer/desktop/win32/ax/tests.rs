use super::*;

#[test]
fn computer_coordinate_click_rejects_controls_with_position_dependent_subregions() {
    for role in [
        ControlType::SplitButton,
        ControlType::TreeItem,
        ControlType::ListItem,
        ControlType::Pane,
        ControlType::Custom,
        ControlType::Document,
    ] {
        assert!(!coordinate_click_supported(role), "{role:?}");
    }
    for role in [
        ControlType::Button,
        ControlType::CheckBox,
        ControlType::RadioButton,
        ControlType::TabItem,
    ] {
        assert!(coordinate_click_supported(role), "{role:?}");
    }
}
