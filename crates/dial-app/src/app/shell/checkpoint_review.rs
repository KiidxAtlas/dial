//! `/review-file`: review checkpointed changes by writer group (main or a worker) and by file.
//! Reverts are token-checked per file and get their own checkpoint for `/undo files`; they are not
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

fn file_label(file: &ReviewFile, reviewed: bool) -> String {
    let mut label = format!(
        "{} [{}] · {}",
        file.path,
        file.writers.join(", "),
        if reviewed { "reviewed" } else { "not reviewed" }
    );
    if let Some(reason) = file.overlap() {
        label.push_str(&format!(" [overlap: {reason}]"));
    }
    label
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ReviewBrowser {
    paths: std::collections::BTreeMap<u64, String>,
    owner: Option<String>,
    overlap_only: bool,
    reviewed: std::collections::BTreeMap<String, String>,
}

impl ReviewBrowser {
    fn key(turn: u64, path: &str) -> String {
        format!("{turn}:{path}")
    }

    fn visible<'a>(&self, review: &'a TurnReview) -> Vec<&'a ReviewFile> {
        review
            .files
            .iter()
            .filter(|file| {
                self.owner
                    .as_ref()
                    .is_none_or(|owner| file.writers.contains(owner))
                    && (!self.overlap_only || file.overlap().is_some())
            })
            .collect()
    }

    fn selected(&self, turn: u64, files: &[&ReviewFile]) -> usize {
        self.paths
            .get(&turn)
            .and_then(|path| files.iter().position(|file| &file.path == path))
            .unwrap_or(0)
    }

    fn adjacent(&mut self, turn: u64, files: &[&ReviewFile], next: bool) -> Option<String> {
        if files.is_empty() {
            return None;
        }
        let selected = self.selected(turn, files);
        let index = if next {
            (selected + 1) % files.len()
        } else {
            (selected + files.len() - 1) % files.len()
        };
        let path = files[index].path.clone();
        self.paths.insert(turn, path.clone());
        Some(path)
    }

    fn save(&self, agent: &mut Agent<'_>) -> Result<()> {
        let mut state = agent.session().control_state();
        state["review_browser"] = serde_json::to_value(self)?;
        agent.session_mut().set_control_state(state)
    }
}

#[derive(PartialEq, Eq)]
enum FileNavigation {
    Back,
    Next,
    Previous,
}

fn latest_changed(checkpoints: &Checkpoints) -> Option<u64> {
    checkpoints
        .list()
        .iter()
        .filter(|turn| turn.files > 0)
        .map(|turn| turn.turn)
        .max()
}

fn validate_preview(checkpoints: &Checkpoints, turn: u64, path: &str, token: &str) -> Result<()> {
    anyhow::ensure!(
        checkpoints.review_file(turn, path)?.1 == token,
        "File changed after preview. Review the new version before keeping it."
    );
    Ok(())
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
        let args = args.trim();
        let (turn_text, requested_path) = args.split_once(' ').unwrap_or((args, ""));
        let turn = if args.is_empty() {
            let Some(turn) = latest_changed(&checkpoints) else {
                let mut text = "No checkpoint changes to review. No files changed.".to_string();
                if let Some(extra) = uncheckpointed(agent) {
                    text.push_str(&format!("\n{extra}"));
                }
                self.note("Review", text);
                return Ok(());
            };
            turn
        } else {
            turn_text
                .parse::<u64>()
                .context("Expected a checkpoint turn number")?
        };
        let state = agent.session().control_state();
        let mut browser: ReviewBrowser =
            serde_json::from_value(state["review_browser"].clone()).unwrap_or_default();
        if !requested_path.trim().is_empty() {
            let review = checkpoints.review_turn(turn)?;
            let files: Vec<_> = review.files.iter().collect();
            browser.paths.insert(turn, requested_path.trim().to_owned());
            self.review_sequence(agent, &checkpoints, &mut browser, turn, &files)?;
        }
        self.review_files(agent, &checkpoints, turn, &mut browser)
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
            "Revert {} file(s) to their state before this turn. The diffs below include any edits you made since.\n{}\nEach file is compared again right before it is written; if one changed, the revert stops, files it already wrote are put back only if they are unchanged since, and the revert checkpoint stays for /undo files. This is not a transaction.",
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
                    "Restored {} file(s). The revert has its own checkpoint for /undo files.",
                    done.len()
                ),
            ),
            Err(error) => self.note("Review", format!("{error:#}")),
        }
    }

    /// Direct file browser; selection, filters and version-specific review marks survive return.
    fn review_files(
        &mut self,
        agent: &mut Agent<'_>,
        checkpoints: &Arc<Checkpoints>,
        mut turn: u64,
        browser: &mut ReviewBrowser,
    ) -> Result<()> {
        loop {
            let review = checkpoints.review_turn(turn)?;
            let files = browser.visible(&review);
            let labels: Vec<_> =
                files
                    .iter()
                    .map(|file| {
                        let reviewed = checkpoints.review_file(turn, &file.path).ok().is_some_and(
                            |(_, token)| {
                                browser.reviewed.get(&ReviewBrowser::key(turn, &file.path))
                                    == Some(&token)
                            },
                        );
                        file_label(file, reviewed)
                    })
                    .collect();
            let mut choices: Vec<_> = labels.iter().map(|label| (label.as_str(), ' ')).collect();
            choices.extend([("Filter writers / overlap", 'f'), ("Choose turn", 't')]);
            if browser.owner.is_some() {
                choices.push(("Revert writer's unshared files", 'r'));
            }
            let mut body = format!(
                "Turn {}: {}\n{} of {} files · writer: {} · overlap only: {}\nReviewed means this exact version was kept, not tested or accepted. Select a file to preview; Esc returns to your draft without changing files.",
                turn,
                review.label,
                files.len(),
                review.files.len(),
                browser.owner.as_deref().unwrap_or("all"),
                browser.overlap_only
            );
            if !review.skipped.is_empty() {
                body.push_str(&format!(
                    "\nToo large to checkpoint (not reviewable or revertible here): {}",
                    review.skipped.join(", ")
                ));
            }
            if let Some(extra) = uncheckpointed(agent) {
                body.push_str(&format!("\n{extra}"));
            }
            let Some(selected) = self.ask_selected(
                "Changed files",
                &body,
                &choices,
                browser.selected(turn, &files),
            ) else {
                return Ok(());
            };
            if selected < files.len() {
                browser.paths.insert(turn, files[selected].path.clone());
                self.review_sequence(agent, checkpoints, browser, turn, &files)?;
            } else if selected == files.len() {
                let groups = review.groups();
                let mut filters = vec![("All writers", 'a')];
                filters.extend(groups.iter().map(|(name, _)| (name.as_str(), ' ')));
                filters.push(("Toggle overlap-only", 'o'));
                if let Some(index) = self.ask(
                    "Review filter",
                    "Writer and overlap filters combine. Filtering never changes files.",
                    &filters,
                ) {
                    if index == filters.len() - 1 {
                        browser.overlap_only = !browser.overlap_only;
                    } else {
                        browser.owner = index.checked_sub(1).map(|i| groups[i].0.clone());
                    }
                    browser.save(agent)?;
                }
            } else if selected == files.len() + 1 {
                let turns = checkpoints.list();
                let labels: Vec<_> = turns
                    .iter()
                    .map(|entry| format!("{}: {} ({} files)", entry.turn, entry.label, entry.files))
                    .collect();
                let options: Vec<_> = labels.iter().map(|label| (label.as_str(), ' ')).collect();
                let current = turns
                    .iter()
                    .position(|entry| entry.turn == turn)
                    .unwrap_or(0);
                if let Some(index) = self.ask_selected(
                    "Choose checkpoint turn",
                    "Esc keeps the current turn and selection.",
                    &options,
                    current,
                ) {
                    turn = turns[index].turn;
                }
            } else if let Some(owner) = &browser.owner {
                let plan = checkpoints.group_plan(turn, owner)?;
                let mut body = format!(
                    "{owner}: {} unshared file(s). No recorded overlap does not prove files are independent. Overlap files are excluded even if hidden by the filter.",
                    plan.safe.len()
                );
                for (path, reason) in &plan.held {
                    body.push_str(&format!("\nHeld: {path} ({reason})"));
                }
                if !plan.safe.is_empty()
                    && self.ask(
                        "Writer revert",
                        &body,
                        &[("Preview revert", 'p'), ("Cancel", 'n')],
                    ) == Some(0)
                {
                    self.revert_group(checkpoints, turn, &plan.safe);
                } else if plan.safe.is_empty() {
                    self.note("Review", body);
                }
            }
        }
    }

    fn review_sequence(
        &mut self,
        agent: &mut Agent<'_>,
        checkpoints: &Arc<Checkpoints>,
        browser: &mut ReviewBrowser,
        turn: u64,
        files: &[&ReviewFile],
    ) -> Result<()> {
        let mut path = browser
            .paths
            .get(&turn)
            .cloned()
            .context("No selected file")?;
        loop {
            browser.paths.insert(turn, path.clone());
            browser.save(agent)?;
            match self.review_one(agent, checkpoints, browser, turn, &path) {
                Ok(FileNavigation::Next) => {
                    path = browser
                        .adjacent(turn, files, true)
                        .context("No next file")?
                }
                Ok(FileNavigation::Previous) => {
                    path = browser
                        .adjacent(turn, files, false)
                        .context("No previous file")?
                }
                Ok(FileNavigation::Back) => return Ok(()),
                Err(error) => {
                    self.note("Review", format!("{error:#}"));
                    return Ok(());
                }
            }
        }
    }

    fn review_one(
        &mut self,
        agent: &mut Agent<'_>,
        checkpoints: &Arc<Checkpoints>,
        browser: &mut ReviewBrowser,
        turn: u64,
        path: &str,
    ) -> Result<FileNavigation> {
        let (diff, token) = checkpoints.review_file(turn, path)?;
        let file = checkpoints
            .review_turn(turn)?
            .files
            .into_iter()
            .find(|file| file.path == path);
        let signal = file.as_ref().and_then(ReviewFile::overlap);
        let key = ReviewBrowser::key(turn, path);
        let reviewed = browser.reviewed.get(&key) == Some(&token);
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
        if let Some(file) = file {
            body.push_str(&format!("Writers: {}\n", file.writers.join(", ")));
        }
        if let Some(extra) = uncheckpointed(agent) {
            body.push_str(&format!("{extra}\n"));
        }
        if let Some(signal) = &signal {
            body.push_str(&format!(
                "Overlap: {signal}. Reverting restores the pre-turn content and may discard that other work.\n"
            ));
        }
        body.push_str("Revert includes user edits shown here; the revert checks the file again right before writing. Page keys scroll the diff/details.");
        match self.ask(
            "Review file",
            &body,
            &[
                ("Keep reviewed version", 'k'),
                ("Revert file", 'r'),
                ("Next file", 'j'),
                ("Previous file", 'u'),
                ("Back to files", 'b'),
            ],
        ) {
            Some(0) => {
                validate_preview(checkpoints, turn, path, &token)?;
                browser.reviewed.insert(key, token);
                browser.save(agent)?;
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
                    return Ok(FileNavigation::Back);
                }
                if self.ask("Revert file?", &format!("Restore {path} to its pre-turn content, including overwriting any user edits shown in the preview? This creates an /undo files checkpoint."), &[("Revert", 'r'), ("Cancel", 'n')]) != Some(0) {
                    return Ok(FileNavigation::Back);
                }
                checkpoints.restore_files(turn, &[(path.to_owned(), token)], true)?;
                browser.reviewed.remove(&key);
                browser.save(agent)?;
                self.note(
                    "Review",
                    "Restored only the selected file. The revert has its own checkpoint for /undo files.",
                );
            }
            Some(2) => return Ok(FileNavigation::Next),
            Some(3) => return Ok(FileNavigation::Previous),
            _ => {}
        }
        Ok(FileNavigation::Back)
    }
}

#[cfg(test)]
mod tests;
