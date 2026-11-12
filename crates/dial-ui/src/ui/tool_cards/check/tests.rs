use super::*;
use crate::llm::model_config::UiConfig;
use crate::ui::theme::Sym;
use crate::ui::theme::{Glyphs, Theme};
use ratatui::text::Line;

fn ctx_with(expanded: bool, f: impl FnOnce(&TranscriptCtx)) {
    let theme = Theme::builtin("dial-dark")
        .unwrap()
        .with_glyphs(Glyphs::Ascii);
    let ui = UiConfig::default();
    f(&TranscriptCtx {
        theme: &theme,
        ui: &ui,
        tick: 0,
        tools_expanded: expanded,
        expand_key: "ctrl+o",
    });
}

fn rendered(rows: &[Line<'_>]) -> Vec<String> {
    rows.iter()
        .map(|r| r.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

fn run(command: &str, output: &str, failed: bool) -> Tool {
    Tool {
        id: "call".into(),
        name: "bash".into(),
        summary: String::new(),
        presentation: Default::default(),
        input: serde_json::json!({ "command": command }).to_string(),
        state: if failed {
            ToolState::Failed
        } else {
            ToolState::Completed
        },
        output: output.into(),
        progress: None,
        elapsed: Some(12.4),
        todo: None,
        agents: Vec::new(),
        outcome: None,
    }
}

const PASSING: &str = "   Compiling dial-ui v0.3.0\n    Finished `test` profile [unoptimized] target(s) in 9.1s\n     Running unittests src/lib.rs\n\nrunning 3 tests\ntest a ... ok\ntest b ... ok\ntest c ... ignored\n\ntest result: ok. 2 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.01s\n\n[exit code: 0]";

#[test]
fn commands_are_classified_as_tests_builds_or_neither() {
    for (command, expected) in [
        ("cargo test -q -p dial-ui", Some(Kind::Test)),
        ("make test-module MOD=llm::retry", Some(Kind::Test)),
        ("python -m pytest tests/", Some(Kind::Test)),
        ("npx vitest run", Some(Kind::Test)),
        (
            "cargo clippy --all-targets -- -D warnings",
            Some(Kind::Build),
        ),
        ("npm run build", Some(Kind::Build)),
        ("./node_modules/.bin/tsc --noEmit", Some(Kind::Build)),
        ("git status", None),
        ("ls tests", None),
    ] {
        assert_eq!(kind(command), expected, "{command}");
    }
}

#[test]
fn a_passing_test_run_is_one_row_with_its_counts() {
    ctx_with(false, |ctx| {
        let rows = rendered(&check_card(&run("cargo test", PASSING, false), 90, ctx).unwrap());
        assert_eq!(rows.len(), 1, "{rows:?}");
        let head = &rows[0];
        assert!(
            head.contains("Test") && head.contains("cargo test"),
            "{head}"
        );
        assert!(
            head.contains("2 passed") && head.contains("1 ignored"),
            "{head}"
        );
        assert!(!head.contains("failed"), "{head}");
    });
}

#[test]
fn failing_tests_are_a_red_card_and_the_log_stays_behind_expand() {
    let output = "running 3 tests\ntest ok_one ... ok\ntest ui::a ... FAILED\ntest ui::b ... FAILED\n\nfailures:\n---- ui::a stdout ----\npanicked\n\ntest result: FAILED. 1 passed; 2 failed; 0 ignored\n\n[exit code: 101]";
    let t = run("cargo test", output, true);
    ctx_with(false, |ctx| {
        let card = check_card(&t, 90, ctx).unwrap();
        let rows = rendered(&card);
        assert!(rows.iter().all(|r| text::width(r) == 90), "{rows:?}");
        assert!(
            rows[0].starts_with(ctx.theme.sym(Sym::BoxTl))
                && rows[0].contains("Test")
                && rows[0].contains("cargo test"),
            "{rows:?}"
        );
        assert!(
            rows[1].contains("ui::a") && rows[2].contains("ui::b"),
            "{rows:?}"
        );
        let foot = rows.last().unwrap();
        assert!(
            foot.contains("1 passed") && foot.contains("2 failed"),
            "{foot}"
        );
        assert!(foot.contains("ctrl+o expands"), "{foot}");
        assert!(!rows.iter().any(|r| r.contains("panicked")), "{rows:?}");
        assert_eq!(card[0].spans[0].style.fg, ctx.theme.fg(Tok::Error).fg);

        // Too narrow for a frame: the same content as indented rows under the header.
        let rows = rendered(&check_card(&t, CARD_MIN - 1, ctx).unwrap());
        assert!(rows[0].contains("Test"), "{rows:?}");
        assert!(rows[1].contains("ui::a"), "{rows:?}");
        assert!(rows.last().unwrap().contains("more lines"), "{rows:?}");
    });
    ctx_with(true, |ctx| assert!(check_card(&t, 90, ctx).is_none()));
}

#[test]
fn build_errors_show_their_location_and_warnings_are_counted() {
    let output = "    Checking dial-ui v0.3.0\nwarning: unused variable: `x`\n --> src/a.rs:3:9\nerror[E0425]: cannot find value `y` in this scope\n --> src/b.rs:7:5\n  |\nerror: could not compile `dial-ui` (lib) due to 1 previous error\n[exit code: 101]";
    ctx_with(false, |ctx| {
        let rows = rendered(&check_card(&run("cargo check", output, true), 90, ctx).unwrap());
        assert!(rows[0].contains("Build"), "{rows:?}");
        let foot = rows.last().unwrap();
        assert!(
            foot.contains("1 error") && foot.contains("1 warning"),
            "{rows:?}"
        );
        assert!(rows[1].contains("error[E0425]"), "{rows:?}");
        assert!(rows[2].contains("src/b.rs:7:5"), "{rows:?}");
        assert!(
            !rows.iter().any(|r| r.contains("could not compile")),
            "{rows:?}"
        );
    });
}

#[test]
fn a_verdict_sums_up_settled_checks_only() {
    let failing = "test result: FAILED. 1 passed; 2 failed; 0 ignored\n[exit code: 101]";
    assert_eq!(
        verdict(&run("cargo test", PASSING, false)),
        Some(Verdict {
            ok: true,
            what: "tests",
            summary: "2 passed".into()
        })
    );
    assert_eq!(
        verdict(&run("cargo test", failing, true)),
        Some(Verdict {
            ok: false,
            what: "tests",
            summary: "2 failed".into()
        })
    );
    let clean = "    Finished `dev` profile target(s) in 3.2s\n[exit code: 0]";
    assert_eq!(
        verdict(&run("cargo clippy", clean, false)).map(|v| (v.ok, v.what, v.summary)),
        Some((true, "build", "clean".to_owned()))
    );
    let moved = "command is running as background job bg1 (after 15s): cargo test";
    assert_eq!(verdict(&run("cargo test", moved, false)), None);
    assert_eq!(verdict(&run("git status", PASSING, false)), None);
}

#[test]
fn a_clean_build_is_one_row() {
    let output =
        "    Checking dial-ui v0.3.0\n    Finished `dev` profile target(s) in 3.2s\n[exit code: 0]";
    ctx_with(false, |ctx| {
        let rows = rendered(&check_card(&run("cargo clippy", output, false), 80, ctx).unwrap());
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(rows[0].contains("clean"), "{rows:?}");
    });
}

#[test]
fn unrecognized_output_and_unattributed_failures_keep_the_ordinary_card() {
    ctx_with(false, |ctx| {
        let moved = "command is running as background job bg1 (after 15s): cargo test";
        assert!(check_card(&run("cargo test", moved, false), 80, ctx).is_none());
        let crashed = "Segmentation fault\n[exit code: 139]";
        assert!(check_card(&run("cargo test", crashed, true), 80, ctx).is_none());
        assert!(check_card(&run("git status", PASSING, false), 80, ctx).is_none());
    });
}

#[test]
fn rows_never_exceed_the_width() {
    ctx_with(false, |ctx| {
        for w in [1, 8, 20, 40, 120] {
            let rows = check_card(
                &run("cargo test --workspace --all-targets", PASSING, false),
                w,
                ctx,
            )
            .unwrap();
            assert!(rows.iter().all(|r| text::line_width(r) <= w), "{w}");
        }
    });
}

#[test]
fn failing_tests_show_why_and_where_under_their_names() {
    let output = "running 2 tests\ntest ui::a ... FAILED\ntest ui::b ... FAILED\n\nfailures:\n---- ui::a stdout ----\n\nthread 'ui::a' (77) panicked at src/a.rs:12:5:\nassertion `left == right` failed\n  left: 1\n right: 2\nnote: run with `RUST_BACKTRACE=1`\n\ntest result: FAILED. 0 passed; 2 failed; 0 ignored\n[exit code: 101]";
    ctx_with(false, |ctx| {
        let rows = rendered(&check_card(&run("cargo test", output, true), 100, ctx).unwrap());
        let at = rows
            .iter()
            .position(|r| r.contains("x ui::a"))
            .expect("test row");
        assert!(
            rows[at + 1].contains("assertion `left == right` failed - src/a.rs:12:5"),
            "{rows:?}"
        );
        // A test whose output names no reason keeps only its name.
        assert!(rows[at + 2].contains("ui::b"), "{rows:?}");
    });
    assert_eq!(
        failure_reason(
            "FAILED tests/t.py::test_x - AssertionError: 1 != 2",
            "test_x"
        ),
        Some(("AssertionError: 1 != 2".to_owned(), None))
    );
}
