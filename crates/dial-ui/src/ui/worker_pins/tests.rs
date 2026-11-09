use super::*;

fn agent(id: &str, status: AgentStatus) -> AgentEntry {
    AgentEntry {
        id: id.to_owned(),
        name: id.to_owned(),
        task: format!("task {id}"),
        status,
        ..AgentEntry::default()
    }
}

/// Board whose snapshot holds `agents`, with totals computed the way the model does.
fn board(agents: Vec<AgentEntry>) -> WorkerBoard {
    let running = agents.iter().filter(|a| !a.status.is_finished()).count();
    let cost_usd = agents.iter().filter_map(|a| a.cost_usd).sum();
    let totals = crate::ui::subagent_model::Totals {
        total: agents.len(),
        running,
        cost_usd,
        ..Default::default()
    };
    WorkerBoard {
        snapshot: HubSnapshot {
            agents,
            totals,
            ..Default::default()
        },
        ..Default::default()
    }
}

#[test]
fn next_recent_starts_at_the_first_and_cycles_then_wraps() {
    let ids = ["a", "b", "c"];
    assert_eq!(next_recent(&ids, None).as_deref(), Some("a"));
    assert_eq!(next_recent(&ids, Some("a")).as_deref(), Some("b"));
    assert_eq!(next_recent(&ids, Some("c")).as_deref(), Some("a"));
}

#[test]
fn next_recent_restarts_when_the_remembered_worker_is_gone_or_pool_empty() {
    assert_eq!(next_recent(&["x"], Some("gone")).as_deref(), Some("x"));
    assert_eq!(next_recent(&[], Some("a")), None);
}

#[test]
fn tally_label_shows_cost_only_when_known() {
    let plain = WorkerTally {
        running: 2,
        total: 3,
        cost_usd: None,
        waiting: 0,
    };
    assert_eq!(plain.label(), "2/3 workers");
    let priced = WorkerTally {
        cost_usd: Some(0.14),
        ..plain
    };
    assert_eq!(priced.label(), "2/3 workers · $0.14");
    let tiny = WorkerTally {
        cost_usd: Some(0.001),
        ..plain
    };
    assert_eq!(tiny.label(), "2/3 workers · $0.0010");
}

#[test]
fn tally_counts_match_snapshot_totals_and_sum_cost() {
    let mut a = agent("a", AgentStatus::Running);
    a.cost_usd = Some(0.1);
    let mut b = agent("b", AgentStatus::Done);
    b.cost_usd = Some(0.04);
    let board = board(vec![a, agent("c", AgentStatus::Waiting), b]);
    let tally = board.tally();
    assert_eq!((tally.running, tally.total, tally.waiting), (2, 3, 1));
    assert!((tally.cost_usd.unwrap() - 0.14).abs() < 1e-9);
}

#[test]
fn observe_primes_then_notifies_once_per_worker_and_outcome() {
    let mut board = board(vec![agent("old", AgentStatus::Done)]);
    board.observe(&board.snapshot.clone());
    assert!(board.take_notices("Alt+A").is_empty(), "priming is silent");

    let running = vec![
        agent("old", AgentStatus::Done),
        agent("w", AgentStatus::Running),
    ];
    board.observe(&HubSnapshot {
        agents: running,
        ..Default::default()
    });
    assert!(board.take_notices("Alt+A").is_empty());

    let finished = vec![
        agent("old", AgentStatus::Done),
        agent("w", AgentStatus::Done),
    ];
    let snap = HubSnapshot {
        agents: finished,
        ..Default::default()
    };
    board.observe(&snap);
    board.observe(&snap);
    assert_eq!(board.take_notices("Alt+A"), vec!["w finished — Alt+A"]);

    // Repeats of the same outcome are silent, but a worker that runs again is announced again.
    let waiting = HubSnapshot {
        agents: vec![agent("x", AgentStatus::Waiting)],
        ..Default::default()
    };
    board.observe(&waiting);
    board.observe(&HubSnapshot {
        agents: vec![agent("x", AgentStatus::Running)],
        ..Default::default()
    });
    board.observe(&waiting);
    board.observe(&waiting);
    assert_eq!(
        board.take_notices("Alt+A"),
        vec!["x needs input — Alt+A", "x needs input — Alt+A"]
    );
}

#[test]
fn interrupted_worker_notice_is_not_cancellation_or_success() {
    let mut board = board(vec![agent("w", AgentStatus::Running)]);
    board.observe(&board.snapshot.clone());
    let snap = HubSnapshot {
        agents: vec![agent("w", AgentStatus::Interrupted)],
        ..Default::default()
    };
    board.observe(&snap);
    board.observe(&snap);
    assert_eq!(board.take_notices("Alt+A"), ["w was interrupted — Alt+A"]);
}

#[test]
fn answered_worker_receipt_does_not_claim_completed_work() {
    let mut board = board(vec![agent("w", AgentStatus::Running)]);
    board.observe(&board.snapshot.clone());
    let snap = HubSnapshot {
        agents: vec![agent("w", AgentStatus::Answered)],
        ..Default::default()
    };
    board.observe(&snap);
    board.observe(&snap);
    assert_eq!(board.take_notices("Alt+A"), ["w replied — Alt+A"]);
}

#[test]
fn observe_notices_failures_and_skips_cancellations() {
    let mut board = board(vec![agent("p", AgentStatus::Running)]);
    board.observe(&board.snapshot.clone());
    board.observe(&HubSnapshot {
        agents: vec![
            agent("p", AgentStatus::Failed),
            agent("q", AgentStatus::Cancelled),
        ],
        ..Default::default()
    });
    assert_eq!(board.take_notices("K"), vec!["p failed — K"]);
}

#[test]
fn rows_hide_when_off_idle_or_narrow() {
    let theme = Theme::dark();
    let busy = board(vec![agent("a", AgentStatus::Running)]);
    let idle = board(vec![agent("a", AgentStatus::Done)]);
    assert!(rows(&busy, 80, 6, PinnedWorkers::Off, true, 0, &theme).is_empty());
    assert!(rows(&idle, 80, 6, PinnedWorkers::Full, true, 0, &theme).is_empty());
    assert!(
        rows(
            &busy,
            MIN_WIDTH - 1,
            6,
            PinnedWorkers::Full,
            true,
            0,
            &theme
        )
        .is_empty()
    );
    assert!(!rows(&busy, 80, 6, PinnedWorkers::Full, true, 0, &theme).is_empty());
}

#[test]
fn collapsed_block_lists_three_and_reports_overflow() {
    let theme = Theme::dark();
    let agents = (0..5)
        .map(|i| agent(&format!("w{i}"), AgentStatus::Running))
        .collect();
    let lines: Vec<String> = rows(
        &board(agents),
        100,
        12,
        PinnedWorkers::Collapsed,
        false,
        0,
        &theme,
    )
    .iter()
    .map(|l| l.to_string())
    .collect();
    assert!(lines[0].to_lowercase().contains("workers") && lines[0].contains("5 active"));
    assert_eq!(lines.len(), 5, "header, three workers, overflow: {lines:?}");
    assert!(lines.last().unwrap().contains("+2 more"));
}

#[test]
fn terminal_rows_gate_is_sixteen() {
    assert_eq!(MIN_TERMINAL_ROWS, 16);
}

#[test]
fn tight_budget_keeps_a_worker_and_prioritizes_input_over_previews() {
    let mut waiting = agent("needs-help", AgentStatus::Waiting);
    waiting.task = "a very long task description that must not hide the state".into();
    let board = board(vec![
        agent("running-1", AgentStatus::Running),
        agent("running-2", AgentStatus::Running),
        waiting,
    ]);
    for theme in [Theme::dark(), Theme::light()] {
        let lines = rows(&board, 24, 3, PinnedWorkers::Full, true, 0, &theme);
        assert_eq!(lines.len(), 3);
        assert!(lines[1].to_string().contains(AgentStatus::Waiting.label()));
        assert!(lines[2].to_string().contains("+2 more"));
        assert!(lines.iter().all(|line| line.width() <= 24));
    }
}

/// Every glyph in the pinned block comes from the theme, so the ASCII preset draws no
/// non-ASCII character in any row shape (spinner, failure, input request, preview, elapsed).
#[test]
fn ascii_glyphs_keep_the_pinned_block_ascii() {
    let theme = Theme::dark().with_glyphs(crate::ui::theme::Glyphs::Ascii);
    let mut running = agent("run", AgentStatus::Running);
    running.steps = 3;
    running.elapsed_ms = Some(ELAPSED_MARKER_MS + 1);
    running.current = Some("reading files".into());
    let mut board = WorkerBoard::default();
    let live = |id: &str| agent(id, AgentStatus::Running);
    board.observe(&snap(vec![live("run"), live("bad"), live("ask")]));
    let now = snap(vec![
        running,
        agent("bad", AgentStatus::Failed),
        agent("ask", AgentStatus::Waiting),
    ]);
    board.observe(&now);
    board.snapshot = now;
    for live_preview in [false, true] {
        let shown = rows(
            &board,
            100,
            12,
            PinnedWorkers::Full,
            live_preview,
            3,
            &theme,
        );
        let text: Vec<String> = shown.iter().map(|l| l.to_string()).collect();
        assert!(text.len() >= 4, "{text:?}");
        assert!(text.iter().all(|r| r.is_ascii()), "{text:?}");
    }
}

fn snap(agents: Vec<AgentEntry>) -> HubSnapshot {
    HubSnapshot {
        agents,
        ..Default::default()
    }
}

fn lines(board: &WorkerBoard) -> Vec<String> {
    rows(
        board,
        100,
        12,
        PinnedWorkers::Full,
        false,
        0,
        &Theme::dark(),
    )
    .iter()
    .map(|line| line.to_string())
    .collect()
}

/// A worker seen running that ends needing the operator stays pinned until it is opened; a
/// clean finish leaves at once, and the header never counts failures as done.
#[test]
fn attention_finishes_stay_pinned_until_opened_and_header_splits_counts() {
    let mut board = WorkerBoard::default();
    let live = |id: &str| agent(id, AgentStatus::Running);
    board.observe(&snap(vec![live("ok"), live("bad"), live("ask")]));
    let ended = snap(vec![
        agent("ok", AgentStatus::Done),
        agent("bad", AgentStatus::Failed),
        agent("ask", AgentStatus::NeedsInput),
    ]);
    board.observe(&ended);
    board.snapshot = ended;
    let shown = lines(&board);
    assert!(
        shown[0].contains("2 need attention") && shown[0].contains("1 done"),
        "{shown:?}"
    );
    assert!(!shown[0].contains("active"), "{shown:?}");
    assert!(shown.iter().any(|l| l.contains("bad")));
    assert!(shown.iter().any(|l| l.contains("ask")));
    assert!(!shown[1..].iter().any(|l| l.contains("ok")));

    board.dismiss("bad");
    board.dismiss("ask");
    assert!(lines(&board).is_empty(), "opened workers leave the block");
}

#[test]
fn failures_from_before_the_board_started_are_not_pinned() {
    let mut board = WorkerBoard::default();
    let old = snap(vec![agent("old", AgentStatus::Failed)]);
    board.observe(&old);
    board.snapshot = old;
    assert!(lines(&board).is_empty());
}

#[test]
fn card_owned_workers_are_left_to_their_card() {
    let mut board = board(vec![
        agent("bg", AgentStatus::Running),
        agent("inline", AgentStatus::Running),
    ]);
    board.card_owned.insert("inline".into());
    board.observe(&board.snapshot.clone());
    let shown = lines(&board);
    assert!(shown[0].contains("1 active") && shown[0].contains("+1 in task cards"));
    assert!(
        !shown[1..].iter().any(|l| l.contains("inline")),
        "{shown:?}"
    );

    // Its transitions are announced by the card, not by a notice.
    board.observe(&snap(vec![
        agent("bg", AgentStatus::Running),
        agent("inline", AgentStatus::Failed),
    ]));
    assert!(board.take_notices("Alt+A").is_empty());
}

#[test]
fn notices_name_the_worker_and_the_key_that_opens_it() {
    let mut board = board(vec![agent("scout", AgentStatus::Running)]);
    board.observe(&board.snapshot.clone());
    let failed = snap(vec![agent("scout", AgentStatus::Failed)]);
    board.observe(&failed);
    board.snapshot = failed;
    assert_eq!(
        board.take_notices_with("Alt+A", "Alt+Shift+A"),
        ["scout failed — open scout: Alt+Shift+A · all workers: Alt+A"]
    );
    assert_eq!(board.take_focus().as_deref(), Some("scout"));
    assert_eq!(board.take_focus(), None, "focus is taken once");
}

#[test]
fn attention_notices_quote_what_the_worker_needs_on_one_line() {
    let mut board = board(vec![
        agent("scout", AgentStatus::Running),
        agent("fixer", AgentStatus::Running),
        agent("done", AgentStatus::Running),
    ]);
    board.observe(&board.snapshot.clone());
    let mut blocked = agent("scout", AgentStatus::Blocked);
    blocked.result = Some("\n  Needs access to ../shared\nsecond line stays out".into());
    let mut asks = agent("fixer", AgentStatus::NeedsInput);
    asks.result = Some(format!("Which branch? {}", "x".repeat(200)));
    let mut done = agent("done", AgentStatus::Done);
    done.result = Some("All finished".into());
    board.observe(&snap(vec![blocked, asks, done]));
    let notices = board.take_notices("Alt+A");
    assert_eq!(
        notices[0],
        "scout is blocked: Needs access to ../shared — Alt+A"
    );
    assert!(
        notices[1].starts_with("fixer needs input: Which branch? x")
            && notices[1].contains("… — Alt+A")
            && notices[1].chars().count() < 100,
        "{notices:?}"
    );
    assert_eq!(
        notices[2], "done finished — Alt+A",
        "a plain finish quotes nothing"
    );
}

#[test]
fn viewing_a_worker_drops_its_notices_and_holds_the_others() {
    let mut board = board(vec![
        agent("seen", AgentStatus::Running),
        agent("other", AgentStatus::Running),
    ]);
    board.observe(&board.snapshot.clone());
    let failed = snap(vec![
        agent("seen", AgentStatus::Failed),
        agent("other", AgentStatus::Failed),
    ]);
    board.observe(&failed);
    board.snapshot = failed.clone();

    // While the viewer shows `seen`, refreshes keep dropping its notices; `other` stays held.
    board.drop_notices_for("seen");
    board.observe(&failed);
    board.drop_notices_for("seen");
    assert_eq!(board.take_notices("Alt+A"), ["other failed — Alt+A"]);
    assert_eq!(board.take_focus().as_deref(), Some("other"));

    // The watched failure counts as seen: it leaves the pinned block; the held one stays.
    let shown = lines(&board);
    assert!(!shown.iter().any(|l| l.contains("seen")), "{shown:?}");
    assert!(shown.iter().any(|l| l.contains("other")), "{shown:?}");
}
