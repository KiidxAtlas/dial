use super::*;

#[test]
fn understanding_offers_every_documented_argument() {
    let choices = command_argument_completions(&Config::default(), &[]);
    for expected in [
        "/understanding architecture",
        "/understanding build",
        "/understanding clear",
        "/understanding query",
    ] {
        assert!(
            choices.iter().any(|choice| choice == expected),
            "missing {expected}"
        );
    }
}
