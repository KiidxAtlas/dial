use super::{Refusal, choose_option, option_list};

fn titles(names: &[&str]) -> Vec<String> {
    names.iter().map(ToString::to_string).collect()
}

#[test]
fn computer_exactly_titled_option_is_chosen_at_its_menu_position() {
    let menu = titles(&[
        "Rich Text Document",
        "",
        "Web Page (.html)",
        "OpenDocument Text",
    ]);
    assert_eq!(choose_option(&menu, "Web Page (.html)"), Ok(2));
}

#[test]
fn computer_case_or_spacing_near_miss_chooses_nothing() {
    let menu = titles(&["JPEG", "jpeg ", "PNG"]);
    assert_eq!(choose_option(&menu, "jpeg"), Err(Refusal::Missing));
    assert_eq!(choose_option(&menu, " PNG"), Err(Refusal::Missing));
}

#[test]
fn computer_shared_title_is_ambiguous() {
    let menu = titles(&["Custom…", "None", "Custom…"]);
    assert_eq!(choose_option(&menu, "Custom…"), Err(Refusal::Ambiguous));
}

#[test]
fn computer_separators_are_neither_chosen_nor_listed() {
    let menu = titles(&["Small", "", "Large"]);
    assert_eq!(choose_option(&menu, ""), Err(Refusal::Missing));
    assert_eq!(option_list(&menu), "\"Small\", \"Large\"");
}
