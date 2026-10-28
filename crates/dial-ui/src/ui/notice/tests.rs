use super::*;
use crate::llm::model_config::UiConfig;

#[test]
fn native_option_shortcuts_are_highlighted_like_alt() {
    let theme = Theme::dark();
    for shortcut in ["Option+M", "Alt+M", "Ctrl+Option+Enter"] {
        assert!(is_chord(shortcut));
        assert!(
            inline(shortcut, theme.fg(Tok::Muted), &theme)
                .iter()
                .any(|span| span.content == shortcut && span.style.fg == theme.fg(Tok::Accent).fg)
        );
    }
}

struct Env {
    theme: Theme,
    ui: UiConfig,
}

impl Env {
    fn new() -> Self {
        Env {
            theme: Theme::dark(),
            ui: UiConfig::default(),
        }
    }
    fn ctx(&self) -> TranscriptCtx<'_> {
        TranscriptCtx {
            theme: &self.theme,
            ui: &self.ui,
            tick: 0,
            tools_expanded: false,
            expand_key: "Ctrl+O",
        }
    }
}

fn plain(rows: &Rows) -> Vec<String> {
    rows.iter()
        .map(|r| r.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

#[test]
fn two_column_rows_hang_wrapped_descriptions_under_their_column() {
    let env = Env::new();
    let body =
        "Commands\n  /review [scope]   read-only review of uncommitted changes against a base ref";
    let rows = rows("Help", body, 40, &env.ctx());
    let text = plain(&rows);
    let desc = text[2]
        .find("read-only")
        .expect("description on the first row");
    assert!(text.len() > 3, "description wraps: {text:?}");
    for continuation in &text[3..] {
        assert_eq!(
            continuation.len() - continuation.trim_start().len(),
            desc,
            "{text:?}"
        );
    }
    assert!(text.iter().all(|r| text::width(r) <= 40), "{text:?}");
}

#[test]
fn json_bodies_read_as_rows_and_empty_ones_say_so() {
    let env = Env::new();
    let empty = plain(&rows("Jobs", "[]", 60, &env.ctx()));
    assert_eq!(empty[1].trim(), EMPTY);
    let text = plain(&rows(
        "Memory",
        r#"{"entries": [], "truncated": false, "last_error": null}"#,
        60,
        &env.ctx(),
    ));
    // serde_json orders keys alphabetically.
    assert_eq!(
        text[1..],
        [
            "  entries: none",
            "  last error: none",
            "  truncated: false"
        ]
    );
}

#[test]
fn values_commands_and_blocked_lines_are_styled_by_meaning() {
    let env = Env::new();
    let theme = &env.theme;
    let rows = rows(
        "Status",
        "Blocked: no debug selector\nRequest timeout: 30s · enabled: true\nRun /reload or press Ctrl+R",
        80,
        &env.ctx(),
    );
    let find = |needle: &str| {
        rows.iter()
            .flat_map(|r| r.spans.iter())
            .find(|s| s.content.contains(needle))
            .unwrap_or_else(|| panic!("{needle} missing"))
            .style
            .fg
    };
    assert_eq!(find("Blocked"), theme.fg(Tok::Warning).fg);
    assert_eq!(find("30s"), theme.fg(Tok::Text).fg);
    assert_eq!(find("Request timeout"), theme.fg(Tok::Muted).fg);
    assert_eq!(find("true"), theme.fg(Tok::Success).fg);
    assert_eq!(find("/reload"), theme.fg(Tok::MdCode).fg);
    assert_eq!(find("Ctrl+R"), theme.fg(Tok::Accent).fg);
    // A filesystem path is not a command.
    let path = super::rows("Dir", "saved in /private/tmp/x", 80, &env.ctx());
    assert!(
        path.iter()
            .flat_map(|r| r.spans.iter())
            .all(|s| s.style.fg != theme.fg(Tok::MdCode).fg)
    );
}

#[test]
fn a_title_with_no_body_says_there_is_nothing_to_show() {
    let env = Env::new();
    assert_eq!(
        plain(&rows("Profiles", "", 40, &env.ctx()))[1].trim(),
        EMPTY
    );
}

#[test]
fn only_section_labels_become_headings() {
    let env = Env::new();
    let body = "Recent projects:\n~/dev/dial · 182 sessions · active now\n  latest 1dc1 · hello\nKeys\n  Ctrl+P  open actions";
    let rows = rows("Archive", body, 80, &env.ctx());
    let heading = |needle: &str| {
        rows.iter()
            .flat_map(|r| r.spans.iter())
            .any(|s| s.content.contains(needle) && s.content == s.content.to_uppercase())
    };
    assert!(heading("RECENT PROJECTS"));
    assert!(
        !heading("RECENT PROJECTS:"),
        "a heading drops its trailing colon"
    );
    assert!(heading("KEYS"));
    assert!(
        !heading("~/dev/dial"),
        "a list row followed by indented detail is not a heading"
    );
}

#[test]
fn received_messages_use_the_irc_lane_without_inventing_a_peer_body() {
    let env = Env::new();
    for ascii in [false, true] {
        let theme = if ascii {
            env.theme
                .clone()
                .with_glyphs(super::super::theme::Glyphs::Ascii)
        } else {
            env.theme.clone()
        };
        let ctx = TranscriptCtx {
            theme: &theme,
            ..env.ctx()
        };
        let worker = "Message from reviewer to parent: Parser looks good.";
        let peer = "Project peer session-42 sent a coordination message.";
        for width in [0, 1, 8, 40, 100] {
            for body in [worker, peer] {
                let rendered = rows("Notice", body, width, &ctx);
                assert!(
                    rendered
                        .iter()
                        .all(|r| text::line_width(r) <= width as usize)
                );
            }
        }
        let worker = plain(&rows("Notice", worker, 100, &ctx));
        assert!(
            worker[0].contains("IRC") && worker[0].contains("reviewer"),
            "{worker:?}"
        );
        assert!(
            worker[0].contains(if ascii { "<-" } else { "⟵" }),
            "{worker:?}"
        );
        assert!(worker[1].contains("Parser looks good."), "{worker:?}");
        let peer = plain(&rows("Notice", peer, 100, &ctx));
        assert_eq!(
            peer.len(),
            1,
            "arrival notices do not carry peer message bodies: {peer:?}"
        );
        assert!(
            peer[0].contains("session-42") && peer[0].contains("coordination message received")
        );
        if ascii {
            assert!(worker.iter().chain(&peer).all(|r| r.is_ascii()));
        }
    }
    let ordinary = plain(&rows(
        "Notice",
        "Message from an unknown source",
        100,
        &env.ctx(),
    ));
    assert!(
        ordinary
            .iter()
            .any(|r| r.contains("Message from an unknown source"))
    );
}
