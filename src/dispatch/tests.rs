use super::*;

#[test]
fn slash_command_mentions_get_a_session_note() {
    let noted = with_session_note("Run /understanding build to map it.\n");
    assert!(
        noted.ends_with("run inside an interactive `dial` session."),
        "{noted}"
    );
    // Paths are not commands.
    assert_eq!(
        with_session_note("Project: /Users/x/proj"),
        "Project: /Users/x/proj"
    );
    assert_eq!(
        with_session_note("Project: /home/x/proj"),
        "Project: /home/x/proj"
    );
}
