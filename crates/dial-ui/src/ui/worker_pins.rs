//! Pinned block above the composer that lists active workers (and finished ones still needing
//! the operator), the `workers` status segment, and finish notices. A `WorkerBoard` holds one
//! throttled `subagent_model::HubCache`, so the block, the status segment and the hub count the
//! same workers. A refresh reads session files, so it runs from the event loop only, never from
//! draw.
//!
//! Workers spawned by a `task` call already show in that call's card in the transcript; the block
//! and the notices leave them to the card so one worker never shows in three places.
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use super::{
    Rows, chrome,
    markdown::truncate_spans,
    subagent_model::{AgentEntry, AgentStatus, HubCache, HubSnapshot, StatusCounts},
    text,
    theme::{Sym, Theme, Tok},
};
use crate::{
    agent::LiveTaskControl, config::paths::Layout, llm::model_config::PinnedWorkers, runtime::Tree,
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

/// What a worker transition asks the user to look at. One notice per worker and outcome until the
/// worker runs again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Outcome {
    Finished,
    Answered,
    Failed,
    NeedsInput,
    Blocked,
    Interrupted,
}

/// Session workers as of the last refresh, plus the transitions still owed a notice.
#[derive(Default)]
pub struct WorkerBoard {
    refreshed_at: Option<Instant>,
    snapshot: HubSnapshot,
    /// Persistent projection state, so each refresh follows transcripts instead of rereading them.
    cache: HubCache,
    primed: bool,
    last: HashMap<String, AgentStatus>,
    /// Outcomes already announced; a worker's entries are cleared when it runs again.
    announced: HashSet<(String, Outcome)>,
    /// `(id, name, outcome, reason)` still owed a notice. The reason is the worker's own question
    /// or blocker, so a notice says what is needed without opening the worker.
    pending: Vec<(String, String, Outcome, Option<String>)>,
    /// Workers a running `task` call card owns (worker bodies whose parent call is still live).
    card_owned: HashSet<String>,
    /// Finished workers the operator already opened; they leave the pinned block.
    dismissed: HashSet<String>,
    /// The worker of the newest notice, which `Alt+Shift+A` opens first.
    focus: Option<String>,
    /// Workers this board saw running. Only they stay pinned after an attention-needing finish,
    /// so failures from before this view started (a restored session) do not crowd the block.
    seen_live: HashSet<String>,
}

/// Running and total workers, with the summed cost once any worker reports one.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct WorkerTally {
    pub running: usize,
    pub total: usize,
    pub cost_usd: Option<f64>,
    /// Workers asking the operator for input (live waiting or ended needing input).
    pub waiting: usize,
}

impl WorkerTally {
    /// `2/3 workers · $0.14`: the cost part is omitted until a worker reports one.
    pub fn label(&self) -> String {
        let mut label = format!("{}/{} workers", self.running, self.total);
        if let Some(cost) = self.cost_usd {
            label.push_str(" · ");
            label.push_str(&text::usd(cost));
        }
        label
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
        let snapshot = self.cache.snapshot(layout, session_id, runtime, live);
        self.card_owned = card_owned(runtime);
        self.observe(&snapshot);
        self.snapshot = snapshot;
        true
    }

    /// Records each worker's status and queues a notice for each new finish, failure or input
    /// request. The first observation only primes the statuses, so old finishes stay silent. A
    /// worker that runs again may be announced again, and leaves the dismissed set.
    fn observe(&mut self, snapshot: &HubSnapshot) {
        for agent in &snapshot.agents {
            let prev = self.last.insert(agent.id.clone(), agent.status);
            if !agent.status.is_finished() {
                self.seen_live.insert(agent.id.clone());
            }
            if prev == Some(agent.status) {
                continue;
            }
            self.dismissed.remove(&agent.id);
            if agent.status == AgentStatus::Running {
                self.announced.retain(|(id, _)| id != &agent.id);
            }
            if !self.primed || self.card_owned.contains(&agent.id) {
                continue;
            }
            let outcome = match agent.status {
                AgentStatus::Done => Outcome::Finished,
                AgentStatus::Answered => Outcome::Answered,
                AgentStatus::Failed => Outcome::Failed,
                AgentStatus::Interrupted => Outcome::Interrupted,
                AgentStatus::Waiting | AgentStatus::NeedsInput => Outcome::NeedsInput,
                AgentStatus::Blocked => Outcome::Blocked,
                AgentStatus::Running | AgentStatus::Cancelled => continue,
            };
            if self.announced.insert((agent.id.clone(), outcome)) {
                let reason = matches!(
                    outcome,
                    Outcome::NeedsInput | Outcome::Blocked | Outcome::Failed
                )
                .then(|| notice_reason(agent.result.as_deref()))
                .flatten();
                self.pending.push((
                    agent.id.clone(),
                    display_name(agent).to_owned(),
                    outcome,
                    reason,
                ));
            }
        }
        self.primed = true;
    }
}

/// Workers a `task` call card owns: those whose parent call is still running. Once the call
/// returns, its card freezes as "continuing in background", so the board announces them again.
fn card_owned(runtime: &Tree) -> HashSet<String> {
    let live_calls: HashSet<String> = runtime
        .bodies("tool")
        .iter()
        .filter_map(|body| {
            body.read(|node| node.result.phase.is_running().then(|| node.id.clone()))
        })
        .collect();
    runtime
        .bodies("worker")
        .iter()
        .filter_map(|body| {
            body.read(|node| {
                live_calls.contains(node.parent.as_ref()?).then_some(())?;
                Some(
                    node.result
                        .data
                        .as_ref()
                        .and_then(|data| data["id"].as_str())
                        .map_or_else(|| node.id.clone(), str::to_owned),
                )
            })
        })
        .collect()
}

/// Longest worker reason quoted in a notice; the hub and viewer show the rest.
const NOTICE_REASON_CHARS: usize = 72;

/// The first line of a worker's question or blocker, clipped for a one-line notice.
fn notice_reason(result: Option<&str>) -> Option<String> {
    let line = text::one_line(result?.lines().find(|line| !line.trim().is_empty())?);
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    Some(if line.chars().count() > NOTICE_REASON_CHARS {
        let kept: String = line.chars().take(NOTICE_REASON_CHARS - 1).collect();
        format!("{}…", kept.trim_end())
    } else {
        line.to_owned()
    })
}

impl WorkerBoard {
    /// Notice lines for transitions since the last drain, e.g. `scout finished — Alt+A`, or
    /// `scout is blocked: needs repo access — Alt+A` when the worker said what it needs.
    pub fn take_notices(&mut self, hub_key: &str) -> Vec<String> {
        self.take_notices_with(hub_key, "")
    }

    /// Notice lines naming the key that opens the worker itself, e.g.
    /// `scout failed — open scout: Alt+Shift+A · all workers: Alt+A`. An empty `open_key` gives
    /// the short [`Self::take_notices`] form. The newest notified worker is what `open_key` opens.
    pub fn take_notices_with(&mut self, hub_key: &str, open_key: &str) -> Vec<String> {
        if let Some((id, _, _, _)) = self.pending.last() {
            self.focus = Some(id.clone());
        }
        self.pending
            .drain(..)
            .map(|(_, name, outcome, reason)| {
                let verb = match outcome {
                    Outcome::Finished => "finished",
                    Outcome::Answered => "replied",
                    Outcome::Failed => "failed",
                    Outcome::NeedsInput => "needs input",
                    Outcome::Blocked => "is blocked",
                    Outcome::Interrupted => "was interrupted",
                };
                let head = match reason {
                    Some(reason) => format!("{name} {verb}: {reason}"),
                    None => format!("{name} {verb}"),
                };
                match (open_key.is_empty(), hub_key.is_empty()) {
                    (true, _) => format!("{head} — {hub_key}"),
                    (false, true) => format!("{head} — open {name}: {open_key}"),
                    (false, false) => {
                        format!("{head} — open {name}: {open_key} · all workers: {hub_key}")
                    }
                }
            })
            .collect()
    }

    /// The operator is watching `id` in the worker viewer: its owed notices are dropped rather
    /// than held (the viewer already shows the change), and its finish counts as seen, so it does
    /// not come back as a pinned attention row when the viewer closes. Other workers' notices
    /// stay queued.
    pub fn drop_notices_for(&mut self, id: &str) {
        self.pending.retain(|(pending, _, _, _)| pending != id);
        self.dismiss(id);
    }

    /// The worker of the newest notice, if it is still listed; taking it clears it, so the next
    /// `Alt+Shift+A` press goes back to cycling.
    pub fn take_focus(&mut self) -> Option<String> {
        let id = self.focus.take()?;
        self.snapshot
            .agents
            .iter()
            .any(|agent| agent.id == id)
            .then_some(id)
    }

    /// The operator opened `id`: a finished worker leaves the pinned block until it runs again.
    pub fn dismiss(&mut self, id: &str) {
        self.dismissed.insert(id.to_owned());
        if self.focus.as_deref() == Some(id) {
            self.focus = None;
        }
    }

    /// Workers as of the last refresh, in roster order.
    pub fn agents(&self) -> &[AgentEntry] {
        &self.snapshot.agents
    }

    /// Recent worker activity as of the last refresh, oldest first.
    pub fn activity(&self) -> &[super::subagent_model::ActivityEvent] {
        &self.snapshot.activity
    }

    /// A board showing `snapshot` without touching disk, for renderer tests.
    #[cfg(any(test, feature = "test-support"))]
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
            waiting: StatusCounts::of(&self.snapshot.agents).needs_input,
        }
    }

    /// Workers the pinned block lists: live ones, then finished ones still needing the operator
    /// (failed, interrupted, blocked, needing input) until opened. Card-owned workers are left
    /// to their card. Attention first, as in the hub roster.
    fn pinned(&self) -> Vec<&AgentEntry> {
        let mut pinned: Vec<&AgentEntry> = self
            .agents()
            .iter()
            .filter(|a| !self.card_owned.contains(&a.id))
            .filter(|a| {
                !a.status.is_finished()
                    || (a.status.attention() < 2
                        && self.seen_live.contains(&a.id)
                        && !self.dismissed.contains(&a.id))
            })
            .collect();
        pinned.sort_by_key(|agent| agent.status.attention());
        pinned
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
    // Input requests and failures must not disappear behind running workers in a collapsed
    // block; `pinned` uses the same attention order as the hub roster.
    let pinned = board.pinned();
    if pinned.is_empty() {
        return Vec::new();
    }
    let fit = |spans| Line::from(truncate_spans(spans, width, theme.sym(Sym::Ellipsis)));
    let own = StatusCounts::of(
        board
            .agents()
            .iter()
            .filter(|a| !board.card_owned.contains(&a.id)),
    );
    let in_cards = StatusCounts::of(
        board
            .agents()
            .iter()
            .filter(|a| board.card_owned.contains(&a.id)),
    )
    .live;
    let attention = pinned.iter().filter(|a| a.status.is_finished()).count();
    let mut header = vec![chrome::label(theme, "workers")];
    let mut counts = Vec::new();
    if own.live > 0 {
        counts.push(format!("{} active", own.live));
    }
    if attention > 0 {
        counts.push(format!(
            "{attention} need{} attention",
            if attention == 1 { "s" } else { "" }
        ));
    }
    if own.done > 0 {
        counts.push(format!("{} done", own.done));
    }
    if in_cards > 0 {
        counts.push(format!("+{in_cards} in task cards"));
    }
    let sep = format!(" {} ", theme.sym(Sym::Dot));
    for count in counts {
        header.push(Span::styled(format!("{sep}{count}"), theme.fg(Tok::Dim)));
    }
    let mut rows = vec![fit(header)];

    // The header takes one row; everything else is workers and at most one overflow row.
    let budget = max - 1;
    let limit = match mode {
        PinnedWorkers::Collapsed => COLLAPSED_WORKERS,
        _ => usize::MAX,
    };
    // Prefer visible workers to previews when the block cannot fit both a worker and overflow.
    let live = live_preview && budget >= 2 && (pinned.len() == 1 || budget >= 3);
    let per_worker = if live { 2 } else { 1 };
    let mut shown = (budget / per_worker).min(limit).min(pinned.len());
    if pinned.len() > shown && shown * per_worker + 1 > budget {
        shown = shown.saturating_sub(1);
    }
    for agent in pinned.iter().take(shown) {
        rows.extend(worker_rows(agent, live, &fit, tick, theme));
    }
    let hidden = pinned.len() - shown;
    if hidden > 0 {
        rows.push(fit(vec![Span::styled(
            format!("  +{hidden} more"),
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
    let (state, marker) = match agent.status {
        AgentStatus::Running => (Tok::Accent, theme.spinner(tick)),
        AgentStatus::Failed => (Tok::Error, theme.sym(Sym::Cross2)),
        _ => (Tok::Warning, theme.sym(Sym::Warn)),
    };
    let sep = format!(" {} ", theme.sym(Sym::Dot));
    // Status comes before free-form names/tasks so requests for input survive narrow widths.
    let mut head = vec![
        Span::styled(
            format!("  {marker} {}{sep}", agent.status.label()),
            theme.fg(state),
        ),
        Span::styled(text::one_line(display_name(agent)), theme.fg(Tok::Accent)),
    ];
    // A step counter only means something once the worker has taken one.
    if agent.steps > 0 {
        head.push(Span::styled(
            format!("{sep}step {}", agent.steps),
            theme.fg(Tok::Dim),
        ));
    }
    if !live && elapsed_ms >= ELAPSED_MARKER_MS {
        head.push(Span::styled(
            format!("{sep}{}", text::duration_label(elapsed_ms as f64 / 1000.0)),
            theme.fg(Tok::Dim),
        ));
    }
    if !agent.task.trim().is_empty() {
        head.push(Span::styled(
            format!("{sep}{}", text::one_line(&agent.task)),
            theme.fg(Tok::Muted),
        ));
    }
    let mut out = vec![fit(head)];
    if live {
        let preview = agent
            .current
            .as_deref()
            .or(agent.result.as_deref())
            .map(text::one_line)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| agent.status.label().to_owned());
        let mut row = vec![Span::styled(
            format!("    {} {preview}", theme.sym(Sym::Ellipsis)),
            theme.fg(Tok::Muted),
        )];
        if elapsed_ms >= ELAPSED_MARKER_MS {
            row.push(Span::styled(
                format!("{sep}{}", text::duration_label(elapsed_ms as f64 / 1000.0)),
                theme.fg(Tok::Dim),
            ));
        }
        out.push(fit(row));
    }
    out
}

#[cfg(test)]
mod tests;
