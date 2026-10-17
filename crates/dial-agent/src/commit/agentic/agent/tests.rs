use super::*;
use crate::commit::run::CommitBackend;
use crate::commit::run::Completion;
use crate::config::types::Config;
use crate::llm::Reply;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Always answers with plain text, so the loop can only end through reminders.
struct TextOnly {
    calls: AtomicUsize,
}

impl CommitBackend for TextOnly {
    fn complete(&self, _config: &Config, _request: &Chat) -> Result<Completion, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Completion {
            reply: Reply {
                text: "done".to_owned(),
                ..Default::default()
            },
            cost_usd: None,
        })
    }
}

#[test]
fn reminders_are_capped_and_leave_state_incomplete() {
    let backend = TextOnly {
        calls: AtomicUsize::new(0),
    };
    let config = Config::default();
    let never_cancelled = std::sync::atomic::AtomicBool::new(false);
    let mut env = ToolEnv {
        root: std::path::Path::new("."),
        config: &config,
        backend: &backend,
        staged: vec!["a.rs".to_owned()],
        numstat: Vec::new(),
        changelog_targets: Vec::new(),
        index: None,
        cancel: &never_cancelled,
        state: Default::default(),
    };
    run_agent(&mut env, None, &[]).expect("loop ends without error");
    assert_eq!(backend.calls.load(Ordering::SeqCst), MAX_REMINDERS + 1);
    assert!(env.state.proposal.is_none() && env.state.split_proposal.is_none());
}

#[test]
fn reminder_lists_missing_items() {
    let text = reminder(&["commit proposal (propose_commit or split_commit)"], 2);
    assert!(text.contains("Missing: commit proposal (propose_commit or split_commit)"));
    assert!(text.contains("Reminder 2 of 3."));
}
