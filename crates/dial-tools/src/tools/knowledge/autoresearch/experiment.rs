//! Branch-scoped autoresearch experiment tracking.
use crate::tools::vcs::git::run_checked;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Experiment {
    pub name: String,
    pub benchmark: String,
    pub metric: String,
    pub maximize: bool,
    pub branch: String,
    /// The branch (or commit) checked out before `init`; `exit` returns there.
    #[serde(default)]
    pub origin: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: i64,
    pub metric: f64,
    pub output: String,
    pub commit: String,
    pub kept: bool,
    pub at: i64,
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn db(path: &Path) -> Result<Connection> {
    fs::create_dir_all(path)?;
    let c = Connection::open(path.join("runs.sqlite"))?;
    c.execute_batch("CREATE TABLE IF NOT EXISTS experiment(name TEXT PRIMARY KEY,benchmark TEXT NOT NULL,metric TEXT NOT NULL,maximize INTEGER NOT NULL,branch TEXT NOT NULL,notes TEXT NOT NULL DEFAULT ''); CREATE TABLE IF NOT EXISTS runs(id INTEGER PRIMARY KEY,experiment TEXT NOT NULL DEFAULT '',metric REAL NOT NULL,output TEXT NOT NULL,commit_id TEXT NOT NULL,kept INTEGER NOT NULL,at INTEGER NOT NULL);")?;
    migrate_runs(&c)?;
    migrate_experiment(&c)?;
    Ok(c)
}
/// Databases created before runs were scoped per experiment lack the `experiment` column.
fn migrate_runs(c: &Connection) -> Result<()> {
    let has_column: i64 = c.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('runs') WHERE name='experiment'",
        [],
        |r| r.get(0),
    )?;
    if has_column > 0 {
        return Ok(());
    }
    c.execute_batch("ALTER TABLE runs ADD COLUMN experiment TEXT NOT NULL DEFAULT '';")?;
    // Legacy runs are attributed only when the single existing experiment makes ownership unambiguous.
    c.execute(
        "UPDATE runs SET experiment=(SELECT name FROM experiment) WHERE experiment='' AND (SELECT COUNT(*) FROM experiment)=1",
        [],
    )?;
    Ok(())
}
/// Databases created before `init` recorded the original branch lack the `origin` column.
fn migrate_experiment(c: &Connection) -> Result<()> {
    let has_column: i64 = c.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('experiment') WHERE name='origin'",
        [],
        |r| r.get(0),
    )?;
    if has_column == 0 {
        c.execute_batch("ALTER TABLE experiment ADD COLUMN origin TEXT NOT NULL DEFAULT '';")?;
    }
    Ok(())
}
/// Refuses unless the working tree matches HEAD: a benchmark must measure the commit it records.
fn ensure_clean(root: &Path, why: &str) -> Result<()> {
    let status = run_checked(root, &["status", "--porcelain"], false)
        .map_err(|error| anyhow::anyhow!("inspecting the working tree failed: {error}"))?;
    let changed = status.lines().count();
    if changed > 0 {
        bail!(
            "{why}, but {changed} path(s) have uncommitted changes. Commit them or stash them (`git stash push -u`), then retry."
        )
    }
    Ok(())
}
/// The checked-out branch name, or the commit itself when HEAD is detached.
fn current_branch(root: &Path) -> Result<String> {
    let name = run_checked(root, &["rev-parse", "--abbrev-ref", "HEAD"], false)
        .map_err(|error| anyhow::anyhow!("reading the current branch failed: {error}"))?;
    let name = name.trim();
    if name != "HEAD" {
        return Ok(name.to_owned());
    }
    let commit = run_checked(root, &["rev-parse", "HEAD"], false)
        .map_err(|error| anyhow::anyhow!("reading the current commit failed: {error}"))?;
    Ok(commit.trim().to_owned())
}
fn slug(name: &str) -> String {
    name.replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "-")
}
/// Best-run ref; must not nest under `autoresearch/<name>`, which is the experiment branch.
fn best_ref(name: &str) -> String {
    format!("autoresearch/{}-best", slug(name))
}
pub fn parse_metric(output: &str, key: &str) -> Option<f64> {
    for line in output.lines().rev() {
        let value = line.strip_prefix("ASI ").unwrap_or(line);
        if let Some((k, v)) = value.split_once('=')
            && k.trim() == key
            && let Ok(n) = v.trim().parse::<f64>()
            && n.is_finite()
        {
            return Some(n);
        }
        if let Ok(j) = serde_json::from_str::<serde_json::Value>(value)
            && let Some(n) = j
                .get(key)
                .and_then(serde_json::Value::as_f64)
                .filter(|n| n.is_finite())
        {
            return Some(n);
        }
    }
    None
}
/// `max` or `min`, case-insensitively. Anything else is refused rather than guessed.
pub fn parse_direction(text: &str) -> Result<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "max" => Ok(true),
        "min" => Ok(false),
        _ => bail!("direction must be max or min, not {text:?}"),
    }
}
pub fn init(
    root: &Path,
    state: &Path,
    name: &str,
    benchmark: &str,
    metric: &str,
    maximize: bool,
) -> Result<Experiment> {
    if name.trim().is_empty() || benchmark.trim().is_empty() || metric.trim().is_empty() {
        bail!("name, benchmark and metric are required")
    }
    let branch = format!("autoresearch/{}", slug(name));
    ensure_clean(root, "initializing autoresearch needs a clean working tree")?;
    let c = db(state)?;
    // Remember where the operator was so `exit` can return there. Re-running init from an experiment
    // branch keeps the origin recorded by the most recent experiment.
    let here = current_branch(root)?;
    let origin = if here.starts_with("autoresearch/") {
        c.query_row(
            "SELECT origin FROM experiment ORDER BY rowid DESC LIMIT 1",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .unwrap_or_default()
    } else {
        here
    };
    let ref_name = format!("refs/heads/{branch}");
    let exists = run_checked(
        root,
        &["show-ref", "--verify", "--quiet", ref_name.as_str()],
        false,
    )
    .is_ok();
    let switched = if exists {
        run_checked(root, &["switch", branch.as_str()], true)
    } else {
        run_checked(root, &["switch", "-c", branch.as_str()], true)
    };
    switched.map_err(|error| anyhow::anyhow!("switching to branch {branch} failed: {error}"))?;
    let e = Experiment {
        name: name.into(),
        benchmark: benchmark.into(),
        metric: metric.into(),
        maximize,
        branch,
        origin,
    };
    c.execute("INSERT INTO experiment(name,benchmark,metric,maximize,branch,origin) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(name) DO UPDATE SET benchmark=excluded.benchmark,metric=excluded.metric,maximize=excluded.maximize,branch=excluded.branch,origin=excluded.origin",params![e.name,e.benchmark,e.metric,e.maximize,e.branch,e.origin])?;
    Ok(e)
}
pub fn current(state: &Path) -> Result<Option<Experiment>> {
    let c = db(state)?;
    let mut s = c.prepare(
        "SELECT name,benchmark,metric,maximize,branch,origin FROM experiment ORDER BY rowid DESC LIMIT 1",
    )?;
    Ok(s.query_row([], |r| {
        Ok(Experiment {
            name: r.get(0)?,
            benchmark: r.get(1)?,
            metric: r.get(2)?,
            maximize: r.get::<_, bool>(3)?,
            branch: r.get(4)?,
            origin: r.get(5)?,
        })
    })
    .optional()?)
}
/// Returns to the branch checked out before `init`. The experiment branch and its runs are kept, so
/// `init` with the same name resumes it.
pub fn exit(root: &Path, state: &Path) -> Result<String> {
    let e = current(state)?.context("no experiment initialized")?;
    ensure_clean(root, "leaving the experiment needs a clean working tree")?;
    if e.origin.is_empty() {
        bail!(
            "no original branch was recorded for {}; switch branches with git directly",
            e.branch
        )
    }
    run_checked(root, &["switch", e.origin.as_str()], true)
        .map_err(|error| anyhow::anyhow!("switching back to {} failed: {error}", e.origin))?;
    Ok(format!(
        "Returned to {}. {} keeps its runs; /autoresearch init {} <metric> <max|min> <benchmark> resumes it.",
        e.origin, e.branch, e.name
    ))
}
pub fn run(root: &Path, state: &Path) -> Result<Run> {
    // Workspace::new applies the tool-call admission boundary (canonical directory, not the
    // filesystem root) and the default strict sandbox, shared with the bash tool.
    run_in(&crate::tools::Workspace::new(root)?, state)
}
/// [`run`] in an existing workspace, so callers with a session keep its shell backend and sandbox.
pub fn run_in(ws: &crate::tools::Workspace, state: &Path) -> Result<Run> {
    let e = current(state)?.context("no experiment initialized")?;
    ensure_clean(
        ws.root(),
        "a benchmark run needs a clean working tree so the recorded commit is what was measured",
    )?;
    let workspace = ws.root();
    let mut command = crate::tools::exec::shell::verifier_command(ws, &e.benchmark)
        .map_err(anyhow::Error::msg)
        .context("building sandboxed benchmark command")?;
    let output = command.output().context("running sandboxed benchmark")?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() {
        bail!("benchmark failed ({}): {}", output.status, text)
    }
    let metric = parse_metric(&text, &e.metric).context(
        "benchmark did not emit requested metric; use `metric=value` or ASI metric=value",
    )?;
    let commit = run_checked(workspace, &["rev-parse", "HEAD"], false)
        .map_err(|error| anyhow::anyhow!("reading Git HEAD failed: {error}"))?;
    let commit = commit.trim().to_owned();
    let mut c = db(state)?;
    c.execute(
        "INSERT INTO runs(experiment,metric,output,commit_id,kept,at) VALUES(?1,?2,?3,?4,0,?5)",
        params![e.name, metric, text, commit, now()],
    )?;
    let id = c.last_insert_rowid();
    let best = c
        .query_row(
            "SELECT metric FROM runs WHERE experiment=?1 AND kept=1 ORDER BY id DESC LIMIT 1",
            params![e.name],
            |r| r.get::<_, f64>(0),
        )
        .optional()?;
    let is_best = best.is_none_or(|v| if e.maximize { metric > v } else { metric < v });
    if is_best {
        // Move the Git ref first: if it fails, the run stays unmarked and DB and Git agree.
        run_checked(
            workspace,
            &["branch", "-f", best_ref(&e.name).as_str(), commit.as_str()],
            true,
        )
        .map_err(|error| anyhow::anyhow!("recording best Git state failed: {error}"))?;
        let tx = c.transaction()?;
        tx.execute(
            "UPDATE runs SET kept=0 WHERE experiment=?1",
            params![e.name],
        )?;
        tx.execute("UPDATE runs SET kept=1 WHERE id=?1", params![id])?;
        tx.commit()?;
    }
    Ok(Run {
        id,
        metric,
        output: text,
        commit,
        kept: is_best,
        at: now(),
    })
}
pub fn log(state: &Path) -> Result<Vec<Run>> {
    let Some(e) = current(state)? else {
        return Ok(Vec::new());
    };
    let c = db(state)?;
    let mut s = c.prepare(
        "SELECT id,metric,output,commit_id,kept,at FROM runs WHERE experiment=?1 ORDER BY id DESC LIMIT 100",
    )?;
    Ok(s.query_map(params![e.name], |r| {
        Ok(Run {
            id: r.get(0)?,
            metric: r.get(1)?,
            output: r.get(2)?,
            commit: r.get(3)?,
            kept: r.get(4)?,
            at: r.get(5)?,
        })
    })?
    .collect::<std::result::Result<_, _>>()?)
}
pub fn update_notes(state: &Path, notes: &str) -> Result<()> {
    let c = db(state)?;
    let n=c.execute("UPDATE experiment SET notes=?1 WHERE name=(SELECT name FROM experiment ORDER BY rowid DESC LIMIT 1)",[notes])?;
    if n == 0 {
        bail!("no experiment initialized")
    };
    Ok(())
}
pub fn notes(state: &Path) -> Result<String> {
    let c = db(state)?;
    Ok(c.query_row(
        "SELECT notes FROM experiment ORDER BY rowid DESC LIMIT 1",
        [],
        |r| r.get(0),
    )
    .optional()?
    .unwrap_or_default())
}

/// What the host did with one self-driving iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The candidate beat the best run: its commit stays and the best ref moved to it.
    Kept,
    /// The candidate measured no better: the branch was reset to the iteration's start.
    Reverted,
    /// The agent changed nothing.
    NoChange,
    /// The benchmark failed or emitted no metric: the branch was reset to the iteration's start.
    Crashed,
}

/// One evaluated iteration of `/autoresearch auto`.
#[derive(Debug, Clone)]
pub struct Iteration {
    pub verdict: Verdict,
    pub metric: Option<f64>,
    /// The candidate commit, also when it was reverted (still reachable through the reflog).
    pub commit: Option<String>,
    /// Benchmark failure or other detail worth showing.
    pub detail: String,
}

/// The commit `HEAD` names; the driver records it before each iteration.
pub fn head(root: &Path) -> Result<String> {
    let commit = run_checked(root, &["rev-parse", "HEAD"], false)
        .map_err(|error| anyhow::anyhow!("reading Git HEAD failed: {error}"))?;
    Ok(commit.trim().to_owned())
}

/// The best recorded metric of the current experiment, if any run has been kept.
pub fn best(state: &Path) -> Result<Option<f64>> {
    let Some(e) = current(state)? else {
        return Ok(None);
    };
    Ok(db(state)?
        .query_row(
            "SELECT metric FROM runs WHERE experiment=?1 AND kept=1 ORDER BY id DESC LIMIT 1",
            params![e.name],
            |r| r.get::<_, f64>(0),
        )
        .optional()?)
}

/// Measure the unchanged branch when no run has been kept yet, so the first candidate must beat
/// the starting code rather than nothing. Returns the baseline run when one was taken.
pub fn ensure_baseline(ws: &crate::tools::Workspace, state: &Path) -> Result<Option<Run>> {
    if best(state)?.is_some() {
        return Ok(None);
    }
    run_in(ws, state)
        .context("measuring the baseline failed")
        .map(Some)
}

/// Host-side judgement of one iteration that started at commit `start`: commit whatever the
/// agent changed on the experiment branch, benchmark it, then keep it only when it beats the best
/// run and otherwise reset the branch to `start`. The agent never decides keep or revert.
pub fn evaluate(
    ws: &crate::tools::Workspace,
    state: &Path,
    start: &str,
    iteration: u32,
) -> Result<Iteration> {
    let root = ws.root();
    let e = current(state)?.context("no experiment initialized")?;
    let branch = current_branch(root)?;
    if branch != e.branch {
        bail!(
            "the working tree is on {branch}, not the experiment branch {}; the loop stopped without touching it",
            e.branch
        )
    }
    let status = run_checked(root, &["status", "--porcelain"], false)
        .map_err(|error| anyhow::anyhow!("inspecting the working tree failed: {error}"))?;
    if !status.trim().is_empty() {
        run_checked(root, &["add", "-A"], true)
            .map_err(|error| anyhow::anyhow!("staging the candidate failed: {error}"))?;
        let message = format!("autoresearch {}: candidate {iteration}", e.name);
        run_checked(
            root,
            &["commit", "-q", "--no-verify", "-m", message.as_str()],
            true,
        )
        .map_err(|error| anyhow::anyhow!("committing the candidate failed: {error}"))?;
    }
    let candidate = head(root)?;
    if candidate == start {
        return Ok(Iteration {
            verdict: Verdict::NoChange,
            metric: None,
            commit: None,
            detail: String::new(),
        });
    }
    let revert = || {
        run_checked(root, &["reset", "-q", "--hard", start], true)
            .map_err(|error| anyhow::anyhow!("reverting to {start} failed: {error}"))
    };
    match run_in(ws, state) {
        Ok(measured) if measured.kept => Ok(Iteration {
            verdict: Verdict::Kept,
            metric: Some(measured.metric),
            commit: Some(candidate),
            detail: String::new(),
        }),
        Ok(measured) => {
            revert()?;
            Ok(Iteration {
                verdict: Verdict::Reverted,
                metric: Some(measured.metric),
                commit: Some(candidate),
                detail: String::new(),
            })
        }
        Err(error) => {
            revert()?;
            Ok(Iteration {
                verdict: Verdict::Crashed,
                metric: None,
                commit: Some(candidate),
                detail: clip(&format!("{error:#}"), 2000),
            })
        }
    }
}

fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let end = text.floor_char_boundary(max);
    format!("{}…", &text[..end])
}

/// Stop rules for `/autoresearch auto`: an iteration cap, and a patience budget of consecutive
/// iterations that did not improve the best run (reverted, crashed or unchanged).
#[derive(Debug, Clone)]
pub struct Policy {
    pub max_iterations: u32,
    pub patience: u32,
    done: u32,
    stale: u32,
}

impl Policy {
    pub const DEFAULT_ITERATIONS: u32 = 10;
    pub const DEFAULT_PATIENCE: u32 = 3;
    pub const MAX_ITERATIONS: u32 = 100;

    /// Parse `[iterations] [--patience N]`; both are optional and must be positive.
    pub fn parse(args: &str) -> Result<Self> {
        let mut policy = Self {
            max_iterations: Self::DEFAULT_ITERATIONS,
            patience: Self::DEFAULT_PATIENCE,
            done: 0,
            stale: 0,
        };
        let mut words = args.split_whitespace();
        let positive = |word: Option<&str>, what: &str| -> Result<u32> {
            match word.map(str::parse::<u32>) {
                Some(Ok(n)) if n > 0 => Ok(n),
                _ => bail!("{what} must be a positive number"),
            }
        };
        while let Some(word) = words.next() {
            if word == "--patience" {
                policy.patience = positive(words.next(), "--patience")?;
            } else {
                policy.max_iterations = positive(Some(word), "iterations")?;
            }
        }
        if policy.max_iterations > Self::MAX_ITERATIONS {
            bail!("at most {} iterations per run", Self::MAX_ITERATIONS)
        }
        Ok(policy)
    }

    /// The 1-based number of the next iteration.
    pub fn next(&self) -> u32 {
        self.done + 1
    }

    /// Record an iteration; returns why the loop stops, if it does.
    pub fn record(&mut self, verdict: Verdict) -> Option<String> {
        self.done += 1;
        self.stale = if verdict == Verdict::Kept {
            0
        } else {
            self.stale + 1
        };
        if self.stale >= self.patience {
            Some(format!(
                "{} iteration(s) in a row without improvement (patience {})",
                self.stale, self.patience
            ))
        } else if self.done >= self.max_iterations {
            Some(format!(
                "reached the cap of {} iteration(s)",
                self.max_iterations
            ))
        } else {
            None
        }
    }
}

/// The instruction the host sends the agent for iteration `number` of `max`.
pub fn iteration_prompt(state: &Path, number: u32, max: u32) -> Result<String> {
    let e = current(state)?.context("no experiment initialized")?;
    let direction = if e.maximize { "maximize" } else { "minimize" };
    let best = best(state)?.map_or_else(|| "none yet".to_owned(), |v| v.to_string());
    let recent = log(state)?
        .into_iter()
        .take(5)
        .map(|r| {
            format!(
                "- #{} {}={}{} at {}",
                r.id,
                e.metric,
                r.metric,
                if r.kept { " (best)" } else { "" },
                &r.commit[..r.commit.len().min(12)]
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let notes = notes(state)?;
    Ok(format!(
        "Autoresearch iteration {number} of {max} for experiment `{name}`: {direction} `{metric}`, measured by `{benchmark}`. Best so far: {best}.\n\
Recent runs:\n{recent}\n{notes_block}\
Make exactly one focused change that you expect to improve `{metric}`, then end your turn. Dial commits it, runs the benchmark, keeps it only if it beats the best, and otherwise reverts it.\n\
Rules: do not commit, switch branches, reset, or stash; do not change the benchmark or the way it measures `{metric}`; do not game the metric; preserve correctness. If you have no promising idea left, change nothing and say so.",
        name = e.name,
        metric = e.metric,
        benchmark = e.benchmark,
        recent = if recent.is_empty() {
            "- none".to_owned()
        } else {
            recent
        },
        notes_block = if notes.trim().is_empty() {
            String::new()
        } else {
            format!("Notes:\n{}\n", notes.trim())
        },
    ))
}
trait OptionalExt<T> {
    fn optional(self) -> rusqlite::Result<Option<T>>;
}
impl<T> OptionalExt<T> for rusqlite::Result<T> {
    fn optional(self) -> rusqlite::Result<Option<T>> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}
#[cfg(test)]
mod tests;
