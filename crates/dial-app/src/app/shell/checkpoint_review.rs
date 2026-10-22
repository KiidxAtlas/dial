//! `/review-file`: review checkpointed changes by writer group (main or a worker) and by file.
//! Reverts are token-checked per file and get their own checkpoint for `/undo`; they are not
//! transactions (see `Checkpoints::restore_files`). Overlap with
//! another writer or a later turn is shown as a risk signal and needs explicit confirmation; its
//! absence is not proof that files are independent.

use crate::agent::Agent;
use crate::app::shell::Shell;
use crate::storage::checkpoint::{Checkpoints, ReviewFile, TurnReview};
use anyhow::{Context, Result};
use std::sync::Arc;

/// What the checkpoint cannot show: shell commands and extension calls outside its observation.
fn uncheckpointed(agent: &Agent<'_>) -> Option<String> {
    let state = agent.session().control_state();
    let edit = &state["reliability"]["edit_loop"];
    let shell = edit["uncheckpointed_shell_commands"].as_u64().unwrap_or(0);
    let ext = edit["uncheckpointed_extension_calls"].as_u64().unwrap_or(0);
    (shell + ext > 0).then(|| {
        format!(
            "Not checkpointed in the latest turn: {shell} shell command(s) and {ext} extension call(s) may have changed files this review cannot show or revert."
        )
    })
}

fn recorded_checks(agent: &Agent<'_>) -> String {
    let state = agent.session().control_state();
    state["reliability"]["verification_results"]
        .as_array()
        .map(|checks| {
            checks
                .iter()
                .map(|check| {
                    format!(
                        "{}: {}",
                        check["name"].as_str().unwrap_or("check"),
                        check["status"].as_str().unwrap_or("unverified")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_else(|| "No checks recorded.".to_string())
}

/// Largest combined diff a group revert will show for one confirmation.
const GROUP_DIFF_LIMIT: usize = 24 * 1024;

fn file_label(file: &ReviewFile) -> String {
    match file.overlap() {
        Some(reason) => format!("{} [overlap: {reason}]", file.path),
        None => format!("{} [{}]", file.path, file.writers.join(", ")),
    }
}

impl Shell {
    pub(in crate::app) fn review_checkpoints(&mut self, agent: &mut Agent<'_>, args: &str) {
        if let Err(error) = self.review_checkpoints_inner(agent, args) {
            self.note("Review", format!("{error:#}"));
        }
    }

    fn review_checkpoints_inner(&mut self, agent: &mut Agent<'_>, args: &str) -> Result<()> {
        let checkpoints = agent
            .checkpoints()
            .cloned()
            .context("No checkpoints are available for this session")?;
        let (turn_text, requested_path) = args.split_once(' ').unwrap_or((args, ""));
        let turn = if args.is_empty() {
            let turns = checkpoints.list();
            if turns.is_empty() {
                let mut text = "No checkpoint changes to review. No files changed.".to_string();
                if let Some(extra) = uncheckpointed(agent) {
                    text = format!("{text}\n{extra}");
                }
                self.note("Review", text);
                return Ok(());
            }
            let labels: Vec<_> = turns
                .iter()
                .map(|entry| format!("{}: {} ({} files)", entry.turn, entry.label, entry.files))
                .collect();
            let choices: Vec<_> = labels.iter().map(|label| (label.as_str(), ' ')).collect();
            let Some(selected) = self.ask(
                "Review checkpoints",
                "Choose a turn. Esc returns to your draft without changing files.",
                &choices,
            ) else {
                return Ok(());
            };
            turns[selected].turn
        } else {
            turn_text
                .parse::<u64>()
                .context("Expected a checkpoint turn number")?
        };
        if !requested_path.trim().is_empty() {
            return self.review_one(agent, &checkpoints, turn, requested_path.trim());
        }
        loop {
            let review = checkpoints.review_turn(turn)?;
            if review.files.is_empty() && review.skipped.is_empty() {
                self.note(
                    "Review",
                    "No readable changed-file inventory. No files changed.",
                );
                return Ok(());
            }
            let groups = review.groups();
            let mut labels: Vec<String> = groups
                .iter()
                .map(|(name, files)| format!("{name}: {} file(s)", files.len()))
                .collect();
            labels.push(format!("All files ({})", review.files.len()));
            let mut body = format!(
                "Turn {}: {}\nChanges are grouped by who wrote them. A file written by several groups, or changed again in a later turn, is marked overlap: reverting it may discard work built on it, so it is always reviewed on its own.",
                review.turn, review.label
            );
            if !review.skipped.is_empty() {
                body.push_str(&format!(
                    "\nToo large to checkpoint (not reviewable or revertible here): {}",
                    review.skipped.join(", ")
                ));
            }
            if let Some(extra) = uncheckpointed(agent) {
                body.push('\n');
                body.push_str(&extra);
            }
            let choices: Vec<_> = labels.iter().map(|label| (label.as_str(), ' ')).collect();
            let Some(selected) = self.ask("Review changes by group", &body, &choices) else {
                return Ok(());
            };
            if selected == groups.len() {
                if !self.review_files(agent, &checkpoints, &review, None)? {
                    return Ok(());
                }
                continue;
            }
            let (owner, _) = &groups[selected];
            let plan = checkpoints.group_plan(turn, owner)?;
            let mut text = format!(
                "{owner}: {} file(s) with no recorded overlap, {} held for individual review.",
                plan.safe.len(),
                plan.held.len()
            );
            for path in &plan.safe {
                text.push_str(&format!("\n  revert  {path}"));
            }
            for (path, why) in &plan.held {
                text.push_str(&format!("\n  held    {path} ({why})"));
            }
            text.push_str("\nNo recorded overlap does not prove files are independent of other code. The revert has one checkpoint for /undo.");
            let mut options = vec![("Review files", 'f'), ("Back", 'b')];
            if !plan.safe.is_empty() {
                options.insert(1, ("Revert unshared files", 'r'));
            }
            let pick = self.ask("Group", &text, &options);
            match pick.map(|i| options[i].0) {
                Some("Review files") => {
                    if !self.review_files(agent, &checkpoints, &review, Some(owner))? {
                        return Ok(());
                    }
                }
                Some("Revert unshared files") => self.revert_group(&checkpoints, turn, &plan.safe),
                _ => {}
            }
        }
    }

    fn revert_group(&mut self, checkpoints: &Arc<Checkpoints>, turn: u64, safe: &[String]) {
        let mut items = Vec::new();
        let mut unreadable = Vec::new();
        for path in safe {
            match checkpoints.review_file(turn, path) {
                Ok((_, token)) => items.push((path.clone(), token)),
                Err(error) => unreadable.push(format!("{path}: {error:#}")),
            }
        }
        if !unreadable.is_empty() {
            self.note(
                "Review",
                format!(
                    "Nothing restored. These files cannot be previewed, so the group cannot be reverted as a whole; review them individually:\n{}",
                    unreadable.join("\n")
                ),
            );
            return;
        }
        // The user must see exactly what changes, including their own later edits to these files.
        let mut diffs = String::new();
        for (path, _) in &items {
            match checkpoints.review_file(turn, path) {
                Ok((diff, _)) => diffs.push_str(&format!("{path}\n{diff}\n")),
                Err(error) => {
                    self.note("Review", format!("Nothing restored. {path} cannot be previewed ({error:#}); review files individually."));
                    return;
                }
            }
        }
        if diffs.len() > GROUP_DIFF_LIMIT {
            self.note(
                "Review",
                format!(
                    "Nothing restored. The combined diff of {} files exceeds the {} KiB group preview limit, so it cannot be confirmed as a whole. Choose Review files and revert them one at a time.",
                    items.len(),
                    GROUP_DIFF_LIMIT / 1024
                ),
            );
            return;
        }
        let body = format!(
            "Revert {} file(s) to their state before this turn. The diffs below include any edits you made since.\n{}\nEach file is compared again right before it is written; if one changed, the revert stops, files it already wrote are put back only if they are unchanged since, and the revert checkpoint stays for /undo. This is not a transaction.",
            items.len(),
            diffs
        );
        if self.ask("Revert group", &body, &[("Revert", 'r'), ("Cancel", 'n')]) != Some(0) {
            return;
        }
        match checkpoints.restore_files(turn, &items, false) {
            Ok(done) => self.note(
                "Review",
                format!(
                    "Restored {} file(s). The revert has its own checkpoint for /undo.",
                    done.len()
                ),
            ),
            Err(error) => self.note("Review", format!("{error:#}")),
        }
    }

    /// File browser for a turn (all files or one writer's). `Ok(false)` ends the whole review.
    fn review_files(
        &mut self,
        agent: &mut Agent<'_>,
        checkpoints: &Arc<Checkpoints>,
        review: &TurnReview,
        owner: Option<&str>,
    ) -> Result<bool> {
        loop {
            let review = checkpoints.review_turn(review.turn)?;
            let files: Vec<&ReviewFile> = review
                .files
                .iter()
                .filter(|f| owner.is_none_or(|o| f.writers.iter().any(|w| w == o)))
                .collect();
            if files.is_empty() {
                return Ok(true);
            }
            let labels: Vec<_> = files.iter().map(|f| file_label(f)).collect();
            let choices: Vec<_> = labels.iter().map(|label| (label.as_str(), ' ')).collect();
            let Some(selected) = self.ask(
                "Changed files",
                "Choose a file. Reviewed, tested and accepted are separate states; file preview shows recorded checks, not new verification. Esc goes back.",
                &choices,
            ) else {
                return Ok(true);
            };
            let path = files[selected].path.clone();
            self.review_one(agent, checkpoints, review.turn, &path)?;
        }
    }

    fn review_one(
        &mut self,
        agent: &mut Agent<'_>,
        checkpoints: &Arc<Checkpoints>,
        turn: u64,
        path: &str,
    ) -> Result<()> {
        let (diff, token) = checkpoints.review_file(turn, path)?;
        let signal = checkpoints
            .review_turn(turn)?
            .files
            .into_iter()
            .find(|f| f.path == path)
            .and_then(|f| f.overlap());
        let state = agent.session().control_state();
        let reviewed = state["reviewed_file"]["identity"].as_str() == Some(token.as_str());
        let mut body = format!(
            "{} · {}\n{}\nRecorded checks (not file-specific proof):\n{}\n",
            path,
            if reviewed {
                "reviewed version"
            } else {
                "not reviewed"
            },
            diff,
            recorded_checks(agent)
        );
        if let Some(signal) = &signal {
            body.push_str(&format!(
                "Overlap: {signal}. Reverting restores the pre-turn content and may discard that other work.\n"
            ));
        }
        body.push_str("Revert includes user edits shown here; the revert checks the file again right before writing. Page keys scroll the diff/details.");
        match self.ask(
            "Review file",
            &body,
            &[("Keep", 'k'), ("Revert file", 'r'), ("Cancel", 'n')],
        ) {
            Some(0) => {
                anyhow::ensure!(
                    checkpoints.review_file(turn, path)?.1 == token,
                    "File changed after preview. Review the new version before keeping it."
                );
                let mut state = agent.session().control_state();
                state["reviewed_file"] =
                    serde_json::json!({"turn":turn,"path":path,"identity":token});
                agent.session_mut().set_control_state(state)?;
                self.note(
                    "Review",
                    "Kept the reviewed version; review is not test verification.",
                );
            }
            Some(1) => {
                if let Some(signal) = &signal
                    && self.ask(
                        "Revert overlapping file?",
                        &format!("{path}: {signal}. Other work may depend on the current content."),
                        &[("Revert anyway", 'r'), ("Cancel", 'n')],
                    ) != Some(0)
                {
                    return Ok(());
                }
                checkpoints.restore_files(turn, &[(path.to_owned(), token)], true)?;
                self.note(
                    "Review",
                    "Restored only the selected file. The revert has its own checkpoint for /undo.",
                );
            }
            _ => {}
        }
        Ok(())
    }
}
