//! Pinned block above the composer that lists active workers, the `workers` status segment, and
//! finish notices. A `WorkerBoard` holds one throttled `subagent_model::snapshot`, so the block,
//! the status segment and the hub count the same workers. A refresh reads session files, so it runs
//! from the event loop only, never from draw.
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use super::{
    Rows, chrome,
    markdown::truncate_spans,
    subagent_model::{AgentEntry, AgentStatus, HubSnapshot, snapshot},
    text,
    theme::{Sym, Theme, Tok},
};
use crate::{
    agent::LiveTaskControl, llm::model_config::PinnedWorkers, runtime::Tree, storage::paths::Layout,
};
use ratatui::text::{Line, Span};

/// How often the board re-reads the session. The hub and viewer poll on their own schedules.
const REFRESH_EVERY: Duration = Duration::from_secs(1);
/// Below this many terminal rows the pinned block stays hidden, so the transcript keeps its room.
pub const MIN_TERMINAL_ROWS: usize = 16;
/// Workers listed in collapsed mode before the overflow row takes over.
const COLLAPSED_WORKERS: usize = 3;
/// Live rows show their elapsed clock once a worker has run this long.
const ELAPSED_MARKER_MS: u64 = 5_000;
/// Narrowest width that still shows the block; below it the block is hidden.
const MIN_WIDTH: usize = 24;

/// What a worker transition asks the user to look at. One notice per worker and outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Outcome {
    Finished,
    Failed,
    NeedsInput,
}

/// Session workers as of the last refresh, plus the transitions still owed a notice.
#[derive(Default)]
pub struct WorkerBoard {
    refreshed_at: Option<Instant>,
    snapshot: HubSnapshot,
    primed: bool,
    last: HashMap<String, AgentStatus>,
    announced: HashSet<(String, Outcome)>,
    pending: Vec<(String, Outcome)>,
}

/// Running and total workers, with the summed cost once any worker reports one.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct WorkerTally {
    pub running: usize,
    pub total: usize,
    pub cost_usd: Option<f64>,
}

impl WorkerTally {
    /// `2/3 workers · $0.14`: the cost part is omitted until a worker reports one.
    pub fn label(&self) -> String {
        let mut label = format!("{}/{} workers", self.running, self.total);
        if let Some(cost) = self.cost_usd {
            label.push_str(" · ");
            label.push_str(&money(cost));
        }
        label
    }
}

fn money(usd: f64) -> String {
    if usd > 0.0 && usd < 0.01 {
        "<$0.01".to_owned()
    } else {
        format!("${usd:.2}")
    }
}

impl WorkerBoard {
    /// Re-reads the session when the last read is at least a second old. Returns whether it ran.
    pub fn refresh(
        &mut self,
        now: Instant,
        layout: &Layout,
        session_id: &str,
        runtime: &Tree,
        live: Option<&LiveTaskControl>,
    ) -> bool {
        if self
            .refreshed_at
            .is_some_and(|at| now.duration_since(at) < REFRESH_EVERY)
        {
            return false;
        }
        self.refreshed_at = Some(now);
        let snapshot = snapshot(layout, session_id, runtime, live);
        self.observe(&snapshot);
        self.snapshot = snapshot;
        true
    }

    /// Records each worker's status and queues a notice for each new finish, failure or input
    /// request. The first observation only primes the statuses, so old finishes stay silent.
    fn observe(&mut self, snapshot: &HubSnapshot) {
        for agent in &snapshot.agents {
            let prev = self.last.insert(agent.id.clone(), agent.status);
            if !self.primed || prev == Some(agent.status) {
                continue;
            }
            let outcome = match agent.status {
                AgentStatus::Done => Outcome::Finished,
                AgentStatus::Failed => Outcome::Failed,
                AgentStatus::Waiting => Outcome::NeedsInput,
                AgentStatus::Running | AgentStatus::Cancelled => continue,
            };
            if self.announced.insert((agent.id.clone(), outcome)) {
                self.pending.push((display_name(agent).to_owned(), outcome));
            }
        }
        self.primed = true;
    }

    /// Notice lines for transitions since the last drain, e.g. `scout finished — Alt+A`.
    pub fn take_notices(&mut self, hub_key: &str) -> Vec<String> {
        self.pending
            .drain(..)
            .map(|(name, outcome)| {
                let verb = match outcome {
                    Outcome::Finished => "finished",
                    Outcome::Failed => "failed",
                    Outcome::NeedsInput => "needs input",
                };
                format!("{name} {verb} — {hub_key}")
            })
            .collect()
    }

    /// Workers as of the last refresh, in roster order.
    pub fn agents(&self) -> &[AgentEntry] {
        &self.snapshot.agents
    }

    /// A board showing `snapshot` without touching disk, for renderer tests.
    #[cfg(test)]
    pub fn from_snapshot(snapshot: HubSnapshot) -> Self {
        Self {
            snapshot,
            primed: true,
            ..Self::default()
        }
    }

    /// Counts for the `workers` segment and the pinned header, from the same snapshot as the hub.
    pub fn tally(&self) -> WorkerTally {
        let totals = &self.snapshot.totals;
        WorkerTally {
            running: totals.running,
            total: totals.total,
            cost_usd: self
                .snapshot
                .agents
                .iter()
                .any(|a| a.cost_usd.is_some())
                .then_some(totals.cost_usd),
        }
    }
}

/// The worker `Alt+Shift+A` opens next: the one after `recent` in `ids`, wrapping around, or the
/// first when `recent` is absent or no longer listed. `None` when `ids` is empty.
pub fn next_recent(ids: &[&str], recent: Option<&str>) -> Option<String> {
    if ids.is_empty() {
        return None;
    }
    let at = recent.and_then(|id| ids.iter().position(|candidate| *candidate == id));
    let next = at.map_or(0, |i| (i + 1) % ids.len());
    Some(ids[next].to_owned())
}

fn display_name(agent: &AgentEntry) -> &str {
    if agent.name.trim().is_empty() {
        agent.id.as_str()
    } else {
        agent.name.as_str()
    }
}

/// Rows for the pinned block, at most `max` tall. Empty when hidden, off, idle or too narrow.
pub fn rows(
    board: &WorkerBoard,
    width: usize,
    max: usize,
    mode: PinnedWorkers,
    live_preview: bool,
    tick: u64,
    theme: &Theme,
) -> Rows {
    if mode == PinnedWorkers::Off || width < MIN_WIDTH || max < 2 {
        return Vec::new();
    }
    let active: Vec<&AgentEntry> = board
        .agents()
        .iter()
        .filter(|a| !a.status.is_finished())
        .collect();
    if active.is_empty() {
        return Vec::new();
    }
    let fit = |spans| Line::from(truncate_spans(spans, width, theme.sym(Sym::Ellipsis)));
    let finished = board.tally().total.saturating_sub(active.len());
    let mut header = vec![
        chrome::label(theme, "workers"),
        Span::styled(format!(" · {} active", active.len()), theme.fg(Tok::Dim)),
    ];
    if finished > 0 {
        header.push(Span::styled(
            format!(" · {finished} done"),
            theme.fg(Tok::Dim),
        ));
    }
    let mut rows = vec![fit(header)];

    // The header takes one row; everything else is workers and at most one overflow row.
    let budget = max - 1;
    let limit = match mode {
        PinnedWorkers::Collapsed => COLLAPSED_WORKERS,
        _ => usize::MAX,
    };
    let live = live_preview && budget >= 2;
    let per_worker = if live { 2 } else { 1 };
    let mut shown = (budget / per_worker).min(limit).min(active.len());
    if active.len() > shown && shown * per_worker + 1 > budget {
        shown = shown.saturating_sub(1);
    }
    for agent in active.iter().take(shown) {
        rows.extend(worker_rows(agent, live, &fit, tick, theme));
    }
    let hidden = active.len() - shown;
    if hidden > 0 {
        rows.push(fit(vec![Span::styled(
            format!("  +{hidden} more running"),
            theme.fg(Tok::Dim),
        )]));
    }
    rows
}

fn worker_rows(
    agent: &AgentEntry,
    live: bool,
    fit: &impl Fn(Vec<Span<'static>>) -> Line<'static>,
    tick: u64,
    theme: &Theme,
) -> Rows {
    let elapsed_ms = agent.elapsed_ms.unwrap_or(0);
    let mut head = vec![
        Span::styled(format!("  {} ", theme.spinner(tick)), theme.fg(Tok::Accent)),
        Span::styled(text::one_line(display_name(agent)), theme.fg(Tok::Accent)),
    ];
    if !agent.task.trim().is_empty() {
        head.push(Span::styled(
            format!(" · {}", text::one_line(&agent.task)),
            theme.fg(Tok::Muted),
        ));
    }
    head.push(Span::styled(
        format!(" · {} · step {}", agent.status.label(), agent.steps),
        theme.fg(Tok::Dim),
    ));
    if !live && elapsed_ms >= ELAPSED_MARKER_MS {
        head.push(Span::styled(clock(elapsed_ms), theme.fg(Tok::Dim)));
    }
    let mut out = vec![fit(head)];
    if live {
        let preview = agent
            .current
            .as_deref()
            .map(text::one_line)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| agent.status.label().to_owned());
        let mut row = vec![Span::styled(
            format!("    {} {preview}", theme.sym(Sym::Ellipsis)),
            theme.fg(Tok::Muted),
        )];
        if elapsed_ms >= ELAPSED_MARKER_MS {
            row.push(Span::styled(
                format!(" · {}", clock(elapsed_ms)),
                theme.fg(Tok::Dim),
            ));
        }
        out.push(fit(row));
    }
    out
}

/// `42s` under a minute, `3m07s` after.
fn clock(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

#[cfg(test)]
mod tests {
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
}
