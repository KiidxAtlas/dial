use super::table;
use dial::storage::session::SessionInfo;

fn info(id: &str, title: &str, messages: usize, project: &str) -> SessionInfo {
    SessionInfo {
        id: id.to_owned(),
        path: Default::default(),
        started: "2020-01-01T00:00:00.000Z".to_owned(),
        updated: "2020-01-01T00:00:00.000Z".to_owned(),
        title: title.to_owned(),
        messages,
        model: String::new(),
        parent_id: None,
        first_message: String::new(),
        last_message: String::new(),
        last_answer: String::new(),
        user_messages: 0,
        project: project.to_owned(),
    }
}

#[test]
fn terminal_session_table_has_a_header_fitted_titles_and_next_steps() {
    let sessions = [
        info(
            "0123456789abcdef",
            "fix the flaky retry test\nand more",
            12,
            "/home/u/p",
        ),
        info("fedcba9876543210", "", 0, "/home/u/p"),
    ];
    let out = table(&sessions, false, 50, false, Some("/home/u"));
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "ID        UPDATED      MSGS  TITLE");
    assert!(lines[1].starts_with("01234567  "), "{out}");
    assert!(lines[1].ends_with("   12  fix the flaky retry…"), "{out}");
    assert!(
        lines[..3]
            .iter()
            .all(|line| dial::ui::text::width(line) < 50),
        "{out}"
    );
    assert!(lines[2].ends_with("(empty session)"), "{out}");
    assert!(out.contains("dial -r <id>"), "{out}");
    assert!(!out.contains('\x1b'), "no colour when disabled: {out:?}");

    let every = table(&sessions, true, 100, false, Some("/home/u"));
    assert!(
        every.lines().next().unwrap().ends_with("PROJECT"),
        "{every}"
    );
    assert!(every.lines().nth(1).unwrap().ends_with("  ~/p"), "{every}");

    let coloured = table(&sessions, false, 60, true, None);
    assert!(coloured.contains("\x1b[36m01234567\x1b[0m"), "{coloured:?}");
}

#[test]
fn an_empty_listing_says_how_to_start_and_where_else_to_look() {
    let out = table(&[], false, 80, false, None);
    assert!(out.starts_with("No sessions in this project yet."), "{out}");
    assert!(out.contains("dial sessions --all"), "{out}");
    let every = table(&[], true, 80, false, None);
    assert!(
        every.starts_with("No sessions yet.") && !every.contains("--all"),
        "{every}"
    );
}
