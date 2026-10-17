//! `/loop` runtime: the argument grammar, the `--while` / `--until` condition gate, and the
//! budget and state machine that re-submits the loop prompt after each settled turn.
//!
//! The loop-limit and loop-condition rules and the loop turn handling. A count or duration limit
//! bounds every turn, including the one the user submits, so `/loop 2` runs two turns.

use crate::tools::shell::ProcessGroup;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

/// Pause between a settled turn and the next iteration, so Esc can land first.
pub(crate) const AUTO_SUBMIT_DELAY: Duration = Duration::from_millis(800);
/// Deadline for one `--while` / `--until` evaluation.
pub(crate) const CONDITION_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the shell checks a running `--while` / `--until` condition for its verdict.
pub(crate) const CONDITION_POLL: Duration = Duration::from_millis(20);

pub(crate) const USAGE: &str = "Usage: /loop [count|duration] [--while|--until '<command>'] [prompt]. Examples: /loop 10, /loop 10m, /loop 20 --until 'bun test' fix the failing tests.";

const LIMIT_REACHED: &str = "Loop limit reached. Loop mode disabled.";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Limit {
    Iterations(u64),
    Duration(Duration),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Condition {
    pub(crate) command: String,
    /// `--until`: continue while the command fails. `--while`: continue while it succeeds.
    pub(crate) until: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Parsed {
    pub(crate) limit: Option<Limit>,
    pub(crate) condition: Option<Condition>,
    pub(crate) prompt: Option<String>,
}

/// Parses `/loop` arguments: an optional leading limit, optional `--while`/`--until`, then the prompt.
pub(crate) fn parse_args(args: &str) -> Result<Parsed, String> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return Ok(Parsed::default());
    }
    let (limit, rest) = take_limit(trimmed)?;
    let (condition, rest) = take_condition(&rest)?;
    Ok(Parsed {
        limit,
        condition,
        prompt: (!rest.is_empty()).then_some(rest),
    })
}

fn split_first(input: &str) -> (&str, &str) {
    match input.find(char::is_whitespace) {
        Some(index) => (&input[..index], input[index..].trim()),
        None => (input, ""),
    }
}

fn take_limit(input: &str) -> Result<(Option<Limit>, String), String> {
    let (token, rest) = split_first(input);
    let lower = token.to_ascii_lowercase();
    let unsigned = lower.strip_prefix(['+', '-']).unwrap_or(lower.as_str());
    // Not a limit attempt (prose such as "keep going", or a leading flag).
    if !unsigned.starts_with(|c: char| c.is_ascii_digit()) {
        return Ok((None, input.to_string()));
    }
    if lower.bytes().all(|b| b.is_ascii_digit()) {
        // A bare integer is a count unless a time unit follows ("10 minutes").
        if !rest.is_empty() {
            let (unit_token, after) = split_first(rest);
            if let Some(unit_ms) = unit_ms(&unit_token.to_ascii_lowercase()) {
                let limit = Limit::Duration(duration_of(&[(lower.as_str(), unit_ms)])?);
                return Ok((Some(limit), after.to_string()));
            }
        }
        let count = positive(&lower, "Loop count must be a positive integer.")?;
        return Ok((Some(Limit::Iterations(count)), rest.to_string()));
    }
    if let Some(segments) = duration_segments(&lower) {
        let mut parts = Vec::with_capacity(segments.len());
        for (amount, unit) in segments {
            let unit_ms =
                unit_ms(unit).ok_or("Loop duration unit must be seconds, minutes, or hours.")?;
            parts.push((amount, unit_ms));
        }
        return Ok((
            Some(Limit::Duration(duration_of(&parts)?)),
            rest.to_string(),
        ));
    }
    // Limit-shaped but unparseable ("-1", "1.5h", "10x10").
    Err(USAGE.to_string())
}

fn unit_ms(unit: &str) -> Option<u64> {
    match unit {
        "s" | "sec" | "secs" | "second" | "seconds" => Some(1_000),
        "m" | "min" | "mins" | "minute" | "minutes" => Some(60_000),
        "h" | "hr" | "hrs" | "hour" | "hours" => Some(3_600_000),
        _ => None,
    }
}

fn positive(text: &str, message: &str) -> Result<u64, String> {
    match text.parse::<u64>() {
        Ok(amount) if (1..=MAX_SAFE_INTEGER).contains(&amount) => Ok(amount),
        _ => Err(message.to_string()),
    }
}

/// Sums `(amount text, unit ms)` segments into a positive duration.
fn duration_of(segments: &[(&str, u64)]) -> Result<Duration, String> {
    let mut total: u64 = 0;
    for (amount, unit_ms) in segments {
        let amount = positive(amount, "Loop duration must be positive.")?;
        let part = amount
            .checked_mul(*unit_ms)
            .ok_or("Loop duration is too large.")?;
        total = total
            .checked_add(part)
            .ok_or("Loop duration is too large.")?;
    }
    if total == 0 {
        return Err("Loop duration must be positive.".to_string());
    }
    Ok(Duration::from_millis(total))
}

/// Splits a compound token such as `1h30m` into `(amount, unit)` pairs; `None` when the token is not
/// shaped like `(\d+[a-z]+)+`.
fn duration_segments(token: &str) -> Option<Vec<(&str, &str)>> {
    let mut out = Vec::new();
    let mut rest = token;
    while !rest.is_empty() {
        let digits = rest.find(|c: char| !c.is_ascii_digit())?;
        if digits == 0 {
            return None;
        }
        let (amount, tail) = rest.split_at(digits);
        let letters = tail
            .find(|c: char| !c.is_ascii_lowercase())
            .unwrap_or(tail.len());
        if letters == 0 {
            return None;
        }
        out.push((amount, &tail[..letters]));
        rest = &tail[letters..];
    }
    (!out.is_empty()).then_some(out)
}

fn take_condition(input: &str) -> Result<(Option<Condition>, String), String> {
    let mut rest = input.trim();
    let mut condition = None;
    while rest.starts_with("--") {
        let body = &rest[2..];
        let run = body
            .find(|c: char| !(c.is_ascii_lowercase() || c == '-'))
            .unwrap_or(body.len());
        let terminated = body[run..]
            .chars()
            .next()
            .is_none_or(|c| c.is_whitespace() || c == '=');
        let name = if run > 0 && body.as_bytes()[0].is_ascii_lowercase() && terminated {
            &rest[..2 + run]
        } else {
            let (token, _) = split_first(rest);
            return Err(format!("Unknown /loop flag {token}. {USAGE}"));
        };
        let until = match name {
            "--while" => false,
            "--until" => true,
            _ => return Err(format!("Unknown /loop flag {name}. {USAGE}")),
        };
        if condition.is_some() {
            return Err("Use only one of --while or --until.".to_string());
        }
        let after_name = &rest[name.len()..];
        let value_text = after_name.strip_prefix('=').unwrap_or(after_name);
        let needs = format!(
            "{name} needs a shell command. Quote it when it contains spaces: /loop {name} 'bun test'."
        );
        let (value, next) = read_shell_word(value_text)
            .map_err(|()| format!("{name} has an unterminated quote."))?
            .filter(|(value, _)| !value.trim().is_empty())
            .ok_or_else(|| needs.clone())?;
        if value_text.trim().starts_with('-') {
            return Err(needs);
        }
        condition = Some(Condition {
            command: value.trim().to_string(),
            until,
        });
        rest = next.trim();
    }
    Ok((condition, rest.to_string()))
}

/// Reads one shell word (quotes and backslash escapes honored) from `input`.
/// Returns the word and the remaining text, `Ok(None)` when there is no word, or `Err(())` for an
/// unterminated quote.
fn read_shell_word(input: &str) -> Result<Option<(String, &str)>, ()> {
    let trimmed = input.trim_start();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let mut word = String::new();
    let mut chars = trimmed.char_indices().peekable();
    let mut end = trimmed.len();
    while let Some((index, c)) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                end = index;
                break;
            }
            '\'' => loop {
                match chars.next() {
                    Some((_, '\'')) => break,
                    Some((_, ch)) => word.push(ch),
                    None => return Err(()),
                }
            },
            '"' => loop {
                match chars.next() {
                    Some((_, '"')) => break,
                    Some((_, '\\')) => match chars.next() {
                        Some((_, ch)) => word.push(ch),
                        None => return Err(()),
                    },
                    Some((_, ch)) => word.push(ch),
                    None => return Err(()),
                }
            },
            '\\' => {
                if let Some((_, ch)) = chars.next() {
                    word.push(ch);
                }
            }
            c => word.push(c),
        }
    }
    Ok(Some((word, &trimmed[end..])))
}

/// Human description of a limit for the enable message.
fn describe_limit(limit: Limit) -> String {
    match limit {
        Limit::Iterations(count) => plural(count, "iteration", "iterations"),
        Limit::Duration(duration) => format_budget(duration),
    }
}

fn plural(count: u64, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// Spelled-out budget in the largest exact unit.
fn format_budget(duration: Duration) -> String {
    let ms = duration.as_millis() as u64;
    if ms.is_multiple_of(3_600_000) {
        plural(ms / 3_600_000, "hour", "hours")
    } else if ms.is_multiple_of(60_000) {
        plural(ms / 60_000, "minute", "minutes")
    } else {
        plural(ms / 1_000, "second", "seconds")
    }
}

fn describe_condition(condition: &Condition) -> String {
    let polarity = if condition.until { "until" } else { "while" };
    format!("{polarity} `{}` succeeds", condition.command)
}

/// Outcome of one condition evaluation.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Continue,
    /// Disable the loop with this message.
    Stop(String),
    /// The evaluation was cancelled (Esc). Stop this iteration quietly.
    Aborted,
}

/// Runs one `--while` / `--until` condition through `sh -c` in `cwd` and maps its exit status.
/// Exit 0 and exit 1 answer the question; anything else, a timeout, or a spawn failure breaks the loop.
pub(crate) fn evaluate(
    condition: &Condition,
    cwd: &Path,
    cancelled: impl Fn() -> bool,
    timeout: Duration,
) -> Verdict {
    let quoted = format!("`{}`", condition.command);
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(&condition.command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // The condition's own children share its group, so cancel and timeout reap all of them.
    let mut group = match crate::platform::process::spawn_group(&mut command) {
        Ok(child) => ProcessGroup::new(child),
        Err(error) => {
            return Verdict::Stop(format!(
                "Loop condition {quoted} could not run: {error}. Loop mode disabled."
            ));
        }
    };
    let stdout = group.child.stdout.take().map(read_in_background);
    let stderr = group.child.stderr.take().map(read_in_background);
    let started = Instant::now();
    let status = loop {
        match group.child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                group.terminate();
                return Verdict::Stop(format!(
                    "Loop condition {quoted} could not run: {error}. Loop mode disabled."
                ));
            }
        }
        if cancelled() {
            group.terminate();
            return Verdict::Aborted;
        }
        if started.elapsed() >= timeout {
            group.terminate();
            return Verdict::Stop(format!(
                "Loop condition {quoted} timed out after {}. Loop mode disabled.",
                format_timeout(timeout)
            ));
        }
        thread::sleep(Duration::from_millis(10));
    };
    let output = format!("{}{}", collect(stdout), collect(stderr));
    match status.code() {
        Some(0) if condition.until => Verdict::Stop(format!(
            "Loop condition {quoted} is now satisfied. Loop mode disabled."
        )),
        Some(0) => Verdict::Continue,
        Some(1) if condition.until => Verdict::Continue,
        Some(1) => Verdict::Stop(format!(
            "Loop condition {quoted} no longer holds. Loop mode disabled."
        )),
        code => {
            let status = code.map_or_else(
                || "no exit status".to_string(),
                |code| format!("exit {code}"),
            );
            let preview = output.lines().map(str::trim).find(|line| !line.is_empty());
            let detail = preview.map_or_else(String::new, |line| format!(": {line}"));
            Verdict::Stop(format!(
                "Loop condition {quoted} failed ({status}){detail}. Loop mode disabled."
            ))
        }
    }
}

/// A `--while` / `--until` condition running on a worker thread. Dropping it cancels the
/// condition, so disabling or pausing the loop never leaves a stray child behind.
#[derive(Debug)]
pub(crate) struct Gate {
    command: String,
    verdicts: Receiver<Verdict>,
    stop: Arc<AtomicBool>,
}

impl Gate {
    /// Starts `condition` on a worker thread. Setting `interrupt` (Esc) or dropping the gate
    /// cancels it; the timeout counts as a failure, as in [`evaluate`].
    pub(crate) fn start(
        condition: &Condition,
        cwd: &Path,
        interrupt: Arc<AtomicBool>,
        timeout: Duration,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, verdicts) = mpsc::channel();
        let worker_condition = condition.clone();
        let worker_cwd = cwd.to_path_buf();
        let worker_stop = stop.clone();
        // A failed spawn drops the worker and its sender; `poll` reports that as a failure.
        let _ = thread::Builder::new()
            .name("loop-condition".into())
            .spawn(move || {
                let cancelled =
                    || interrupt.load(Ordering::SeqCst) || worker_stop.load(Ordering::SeqCst);
                let verdict = evaluate(&worker_condition, &worker_cwd, cancelled, timeout);
                let _ = tx.send(verdict);
            });
        Self {
            command: condition.command.clone(),
            verdicts,
            stop,
        }
    }

    /// The verdict, once the worker has finished.
    fn poll(&self) -> Option<Verdict> {
        match self.verdicts.try_recv() {
            Ok(verdict) => Some(verdict),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Verdict::Stop(format!(
                "Loop condition `{}` could not run. Loop mode disabled.",
                self.command
            ))),
        }
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn format_timeout(timeout: Duration) -> String {
    let ms = timeout.as_millis();
    if ms.is_multiple_of(1_000) {
        format!("{}s", ms / 1_000)
    } else {
        format!("{ms}ms")
    }
}

pub(super) fn read_in_background<R: Read + Send + 'static>(mut pipe: R) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = pipe.read_to_end(&mut buffer);
        let _ = tx.send(String::from_utf8_lossy(&buffer).into_owned());
    });
    rx
}

/// Collects a background reader. A grandchild may hold the pipe open, so the wait is bounded.
pub(super) fn collect(stream: Option<Receiver<String>>) -> String {
    stream
        .and_then(|rx| rx.recv_timeout(Duration::from_millis(200)).ok())
        .unwrap_or_default()
}

/// Budget runtime.
#[derive(Clone, Copy, Debug)]
enum Budget {
    Iterations { initial: u64, remaining: u64 },
    Deadline { total: Duration, at: Instant },
}

impl Budget {
    fn new(limit: Limit, now: Instant) -> Self {
        match limit {
            Limit::Iterations(count) => Budget::Iterations {
                initial: count,
                remaining: count,
            },
            Limit::Duration(total) => Budget::Deadline {
                total,
                at: now + total,
            },
        }
    }

    fn exhausted(&self, now: Instant) -> bool {
        match *self {
            Budget::Iterations { remaining, .. } => remaining == 0,
            Budget::Deadline { at, .. } => now >= at,
        }
    }

    /// Spends one iteration. `false` when the budget is already spent.
    fn consume(&mut self, now: Instant) -> bool {
        match self {
            Budget::Iterations { remaining, .. } if *remaining > 0 => {
                *remaining -= 1;
                true
            }
            Budget::Iterations { .. } => false,
            Budget::Deadline { at, .. } => now < *at,
        }
    }

    fn describe(&self) -> String {
        match *self {
            Budget::Iterations { initial, remaining } => format!(
                "{remaining} of {} remaining",
                plural(initial, "iteration", "iterations")
            ),
            Budget::Deadline { total, .. } => format!("{} limit", format_budget(total)),
        }
    }
}

/// What the run loop should do with the loop at a given instant.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Due {
    /// Nothing to do.
    Idle,
    /// Submit this prompt as the next iteration.
    Submit(String),
    /// The loop has ended; show this message.
    Stop(String),
    /// The iteration was cancelled; the loop is suspended and stays enabled.
    Paused(String),
}

/// In-memory `/loop` state. Not persisted: a resumed session starts with the loop off.
#[derive(Debug, Default)]
pub(crate) struct LoopMode {
    enabled: bool,
    paused: bool,
    prompt: Option<String>,
    budget: Option<Budget>,
    condition: Option<Condition>,
    due_at: Option<Instant>,
    /// Set when the loop submits an iteration, cleared when that turn starts.
    submitting: bool,
    /// A running `--while` / `--until` condition, if any. Dropping it cancels the condition.
    gate: Option<Gate>,
}

impl LoopMode {
    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// The instant the next iteration becomes due, if one is armed.
    pub(crate) fn next_due(&self) -> Option<Instant> {
        self.due_at
    }

    /// Whether a condition is running. The shell polls more often meanwhile so its verdict lands promptly.
    pub(crate) fn is_awaiting_condition(&self) -> bool {
        self.gate.is_some()
    }

    /// Turns the loop on for `parsed` and returns the status message.
    pub(crate) fn enable(&mut self, parsed: &Parsed, now: Instant) -> String {
        *self = LoopMode {
            enabled: true,
            budget: parsed.limit.map(|limit| Budget::new(limit, now)),
            condition: parsed.condition.clone(),
            ..LoopMode::default()
        };
        self.enabled = true;
        let limit = parsed.limit.map_or_else(String::new, |limit| {
            format!(" Limited to {}.", describe_limit(limit))
        });
        let remaining = self
            .budget
            .map_or_else(String::new, |budget| format!(" {}.", budget.describe()));
        let condition = self
            .condition
            .as_ref()
            .map_or_else(String::new, |condition| {
                format!(" Continuing {}.", describe_condition(condition))
            });
        let tail = if parsed.prompt.is_some() {
            "Repeating it after each turn."
        } else {
            "Your next prompt will repeat after each turn."
        };
        format!(
            "Loop mode enabled.{limit}{remaining}{condition} {tail} Esc suspends the ongoing loop; /loop again to disable."
        )
    }

    /// Turns the loop off. Returns whether it was on.
    pub(crate) fn disable(&mut self) -> bool {
        let was = self.enabled;
        *self = LoopMode::default();
        was
    }

    /// Suspends the loop: drops the loop prompt and any pending iteration, but leaves it enabled.
    /// The next manual prompt becomes the new loop prompt and resumes it.
    pub(crate) fn pause(&mut self) {
        self.prompt = None;
        self.paused = true;
        self.due_at = None;
        self.submitting = false;
        self.gate = None;
    }

    /// Called when a turn starts. A loop-submitted turn is already accounted for. A manual
    /// prompt becomes the loop prompt, resumes a paused loop, and spends one iteration.
    pub(crate) fn turn_started(&mut self, text: &str) {
        self.due_at = None;
        self.gate = None;
        if !self.enabled || std::mem::take(&mut self.submitting) {
            return;
        }
        self.prompt = Some(text.to_string());
        self.paused = false;
        if let Some(Budget::Iterations { remaining, .. }) = &mut self.budget {
            *remaining = remaining.saturating_sub(1);
        }
    }

    /// Called when a turn settles. An interrupted or failed turn suspends the loop. Otherwise the
    /// next iteration is armed after [`AUTO_SUBMIT_DELAY`].
    pub(crate) fn turn_settled(&mut self, now: Instant, halted: bool) {
        if !self.enabled {
            return;
        }
        if halted {
            self.pause();
            return;
        }
        if self.prompt.is_some() && !self.paused {
            self.due_at = Some(now + AUTO_SUBMIT_DELAY);
        }
    }

    /// Decides the armed iteration. Checks run in order: budget exhausted, condition start, then
    /// (once the verdict is in) budget consume. A `--while` / `--until` condition runs on a worker
    /// through `start`; the iteration waits in `Due::Idle` until its verdict is ready.
    pub(crate) fn take_due(&mut self, now: Instant, start: impl FnOnce(&Condition) -> Gate) -> Due {
        if let Some(gate) = &self.gate {
            let Some(verdict) = gate.poll() else {
                return Due::Idle;
            };
            self.gate = None;
            return self.admit(now, verdict);
        }
        match self.due_at {
            Some(due_at) if now >= due_at => self.due_at = None,
            _ => return Due::Idle,
        }
        if !self.enabled || self.paused {
            return Due::Idle;
        }
        if self.prompt.is_none() {
            return Due::Idle;
        }
        if self.budget.is_some_and(|budget| budget.exhausted(now)) {
            return self.stop(LIMIT_REACHED.to_string());
        }
        if let Some(condition) = self.condition.clone() {
            self.gate = Some(start(&condition));
            return Due::Idle;
        }
        self.admit(now, Verdict::Continue)
    }

    /// Applies a condition verdict, spends one iteration, and submits the loop prompt.
    fn admit(&mut self, now: Instant, verdict: Verdict) -> Due {
        match verdict {
            Verdict::Continue => {}
            Verdict::Aborted => {
                self.pause();
                return Due::Paused("Loop paused. Send a prompt to resume.".to_string());
            }
            Verdict::Stop(message) => return self.stop(message),
        }
        let Some(prompt) = self.prompt.clone() else {
            return Due::Idle;
        };
        if let Some(budget) = &mut self.budget
            && !budget.consume(now)
        {
            return self.stop(LIMIT_REACHED.to_string());
        }
        self.submitting = true;
        Due::Submit(prompt)
    }

    fn stop(&mut self, message: String) -> Due {
        self.disable();
        Due::Stop(message)
    }
}

#[cfg(test)]
mod tests;
