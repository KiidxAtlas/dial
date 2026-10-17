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
    assert_eq!(tiny.label(), "2/3 workers · <$0.01");
}

#[test]
fn tally_counts_match_snapshot_totals_and_sum_cost() {
    let mut a = agent("a", AgentStatus::Running);
    a.cost_usd = Some(0.1);
    let mut b = agent("b", AgentStatus::Done);
    b.cost_usd = Some(0.04);
    let board = board(vec![a, agent("c", AgentStatus::Waiting), b]);
    let tally = board.tally();
    assert_eq!((tally.running, tally.total), (2, 3));
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

    // Waiting, then finished again later, is a new outcome; repeats of the same are not.
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
    assert_eq!(board.take_notices("Alt+A"), vec!["x needs input — Alt+A"]);
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
    assert!(lines.last().unwrap().contains("+2 more running"));
}

#[test]
fn terminal_rows_gate_is_sixteen() {
    assert_eq!(MIN_TERMINAL_ROWS, 16);
}
