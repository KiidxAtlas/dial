//! What Dial tells the hosting terminal about the session outside the drawn cells: dock attention
//! while it waits (iTerm2), OSC 9;4 progress, `SetUserVar` values for status bars and badges, and
//! the working directory (OSC 7). The tab's state label lives in the title, see
//! [`Activity::label`]. Each paint writes only what changed; [`Signals`] the terminal does not
//! support are never written, and [`SignalWriter::reset`] puts progress, attention and variables
//! back when Dial leaves.
use std::path::PathBuf;

use base64::Engine as _;

use super::termcaps::Integration;

/// What the user needs to respond to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prompt {
    Approval,
    Question,
}

/// What the session is doing, as the tab shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Activity {
    #[default]
    Idle,
    /// A turn, compaction or a local-compute wait is running.
    Working,
    /// A foreground test command is pending.
    Testing,
    /// A prompt waits for the user (also a turn that ended asking for input).
    Waiting(Prompt),
    /// The last turn finished; cleared by the next key press or turn.
    Done,
    /// The completed turn recorded file changes to review; not a verdict on its checks.
    ReviewReady,
    /// The last turn failed, was blocked or is incomplete.
    Failed,
}

impl Activity {
    /// The stable, coarse `dialState` value consumed by status bars.
    pub fn name(self) -> &'static str {
        match self {
            Activity::Idle => "idle",
            Activity::Working | Activity::Testing => "working",
            Activity::Waiting(_) => "waiting",
            Activity::Done | Activity::ReviewReady => "done",
            Activity::Failed => "failed",
        }
    }

    /// The state left by a turn, given its observed changed-file count.
    pub fn after_turn(outcome: crate::runtime::Phase, changed_files: usize) -> Self {
        use crate::runtime::Phase;
        match outcome {
            Phase::Completed if changed_files > 0 => Activity::ReviewReady,
            Phase::Completed => Activity::Done,
            Phase::NeedsInput => Activity::Waiting(Prompt::Question),
            Phase::Failed | Phase::Blocked | Phase::Incomplete => Activity::Failed,
            Phase::Running | Phase::Interrupted | Phase::Answered => Activity::Idle,
        }
    }

    /// The rightmost text in the tab, also shown by terminals without emoji.
    pub fn label(self) -> &'static str {
        match self {
            Activity::Idle => "",
            Activity::Working => "Working",
            Activity::Testing => "Testing",
            Activity::Waiting(Prompt::Approval) => "Approve?",
            Activity::Waiting(Prompt::Question) => "Question",
            Activity::Done => "Done",
            Activity::ReviewReady => "Review ready",
            Activity::Failed => "Failed",
        }
    }

    /// Optional color before the label in a Unicode title.
    pub fn marker(self) -> &'static str {
        match self {
            Activity::Idle => "",
            Activity::Working | Activity::Testing => "\u{1F7E0}",
            Activity::Waiting(_) => "\u{1F535}",
            Activity::Failed => "\u{1F534}",
            Activity::Done | Activity::ReviewReady => "\u{1F7E2}",
        }
    }
}

/// OSC 9;4 progress shown in the tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Progress {
    #[default]
    None,
    Indeterminate,
    Percent(u8),
    /// Work is held (waiting for the user); keeps the last percentage.
    Paused,
}

/// The desired terminal state for one paint.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Signals {
    pub activity: Activity,
    pub progress: Progress,
    /// Absolute project directory for OSC 7.
    pub cwd: Option<PathBuf>,
    /// `SetUserVar` name and value pairs (values are sanitized and bounded when written).
    pub vars: Vec<(&'static str, String)>,
}

/// Longest user-variable value written.
const VAR_LIMIT: usize = 200;

/// Tracks what the terminal currently shows and writes the difference.
#[derive(Clone, Debug)]
pub struct SignalWriter {
    caps: Integration,
    host: String,
    sent: Signals,
}

impl SignalWriter {
    pub fn new(caps: Integration) -> Self {
        let host = if caps.cwd {
            crate::platform::sys::hostname().unwrap_or_default()
        } else {
            String::new()
        };
        Self {
            caps,
            host,
            sent: Signals::default(),
        }
    }

    pub fn caps(&self) -> Integration {
        self.caps
    }

    /// True when writing `want` would change nothing.
    pub fn is_current(&self, want: &Signals) -> bool {
        self.supported(want) == self.sent
    }

    /// True when the terminal shows none of Dial's state (the working directory aside).
    pub fn is_clear(&self) -> bool {
        self.sent.activity == Activity::Idle
            && self.sent.progress == Progress::None
            && self.sent.vars.is_empty()
    }

    /// The bytes that move the terminal from what it shows to `want`.
    pub fn update(&mut self, want: &Signals) -> String {
        let (out, next) = self.prepare(Some(want));
        self.accept(next);
        out
    }

    /// Stage a diff without advancing the cache before an asynchronous frame is accepted.
    pub(crate) fn prepare(&self, want: Option<&Signals>) -> (String, Signals) {
        let next = want.map_or_else(
            || Signals {
                cwd: self.sent.cwd.clone(),
                ..Signals::default()
            },
            |want| self.supported(want),
        );
        (self.diff(&next), next)
    }

    pub(crate) fn accept(&mut self, next: Signals) {
        self.sent = next;
    }

    fn diff(&self, want: &Signals) -> String {
        let caps = self.caps;
        let mut out = String::new();
        let waiting = |activity: Activity| matches!(activity, Activity::Waiting(_));
        if caps.attention && waiting(want.activity) != waiting(self.sent.activity) {
            out.push_str(&caps.wrap(&attention(waiting(want.activity))));
        }
        if want.progress != self.sent.progress {
            out.push_str(&caps.wrap(&progress(want.progress)));
        }
        for (name, value) in &want.vars {
            let before = self.sent.vars.iter().find(|(sent, _)| sent == name);
            if before.is_none_or(|(_, sent)| sent != value) {
                out.push_str(&caps.wrap(&user_var(name, value)));
            }
        }
        for (name, _) in &self.sent.vars {
            if !want.vars.iter().any(|(wanted, _)| wanted == name) {
                out.push_str(&caps.wrap(&user_var(name, "")));
            }
        }
        if want.cwd != self.sent.cwd
            && let Some(url) = want
                .cwd
                .as_deref()
                .and_then(|cwd| super::term_links::file_url(cwd, &self.host, None))
        {
            // tmux keeps the pane's directory from OSC 7 itself, so it is never wrapped.
            out.push_str(&format!("\x1b]7;{url}\x1b\\"));
        }
        out
    }

    /// The bytes that restore progress, attention and variables, without recording it
    /// (the panic hook writes this).
    pub fn reset_sequence(&self) -> String {
        self.prepare(None).0
    }

    /// Restore the terminal and record that it shows nothing of Dial's.
    pub fn reset(&mut self) -> String {
        let (out, next) = self.prepare(None);
        self.accept(next);
        out
    }

    /// `want` with everything this terminal cannot show removed, values sanitized.
    fn supported(&self, want: &Signals) -> Signals {
        let caps = self.caps;
        // The terminal attention protocol has one bit, not different prompt kinds.
        let activity = if caps.attention && matches!(want.activity, Activity::Waiting(_)) {
            Activity::Waiting(Prompt::Question)
        } else {
            Activity::Idle
        };
        Signals {
            activity,
            progress: if caps.progress {
                want.progress
            } else {
                Progress::None
            },
            cwd: want.cwd.clone().filter(|cwd| caps.cwd && cwd.is_absolute()),
            vars: if caps.user_vars {
                want.vars
                    .iter()
                    .map(|(name, value)| (*name, bounded(value)))
                    .collect()
            } else {
                Vec::new()
            },
        }
    }
}

fn bounded(value: &str) -> String {
    super::text::one_line(value)
        .chars()
        .take(VAR_LIMIT)
        .collect()
}

fn attention(on: bool) -> String {
    format!(
        "\x1b]1337;RequestAttention={}\x07",
        if on { "yes" } else { "no" }
    )
}

fn progress(progress: Progress) -> String {
    match progress {
        Progress::None => "\x1b]9;4;0\x07".to_owned(),
        Progress::Indeterminate => "\x1b]9;4;3\x07".to_owned(),
        Progress::Percent(percent) => format!("\x1b]9;4;1;{}\x07", percent.min(100)),
        Progress::Paused => "\x1b]9;4;4\x07".to_owned(),
    }
}

fn user_var(name: &str, value: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(value);
    format!("\x1b]1337;SetUserVar={name}={encoded}\x07")
}

#[cfg(test)]
mod tests;
