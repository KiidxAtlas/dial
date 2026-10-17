//! `/omfg <complaint>`: author a TTSR rule from a complaint, check it against this session,
//! and save it to the project or user rules directory.

use crate::agent::Agent;
use crate::agent::modes::omfg::{Candidate, Evaluation, MAX_ATTEMPTS, build_prompt};
use crate::app::{App, Shell, Step};
use crate::ui::{text, theme::Tok};
use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, read};
use ratatui::text::{Line, Span};

const CHOICE_PROJECT: usize = 0;
const CHOICE_GLOBAL: usize = 1;
const CHOICE_AMEND: usize = 2;

pub(super) fn run(shell: &mut Shell, app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
    let complaint = args.trim();
    if complaint.is_empty() {
        shell.note("OMFG", "Usage: /omfg <complaint>");
        return Ok(Step::Done);
    }
    if !shell.wait_compute(app)? {
        return Ok(Step::Done);
    }

    let mut feedback = String::new();
    let mut previous: Option<Candidate> = None;
    'request: loop {
        let mut matched: Option<Candidate> = None;
        let mut unmatched: Option<Candidate> = None;
        for attempt in 1..=MAX_ATTEMPTS {
            let previous_json = previous.as_ref().map(|c| c.json.as_str());
            let prompt = build_prompt(complaint, &feedback, previous_json);
            let reply = match agent.omfg_generate(&prompt) {
                Ok(reply) => reply,
                Err(error) => {
                    shell.note("OMFG", format!("{error:#}"));
                    return Ok(Step::Done);
                }
            };
            match agent.omfg_evaluate(&reply) {
                Evaluation::Invalid(reason) => {
                    feedback.push_str(&format!(
                        "Attempt {attempt} failed: invalid rule ({reason})\n"
                    ));
                }
                Evaluation::Unmatched(candidate) => {
                    feedback.push_str(&format!(
                        "Attempt {attempt} failed validation: the candidate did not match any assistant output in this conversation.\nFailed candidate:\n{}\n",
                        candidate.json
                    ));
                    unmatched = Some(candidate);
                }
                Evaluation::Matched(candidate) => {
                    matched = Some(candidate);
                    break;
                }
            }
        }

        let candidate = match (matched, unmatched) {
            (Some(candidate), _) => candidate,
            (None, Some(candidate)) => {
                match shell.ask(
                    "Validation",
                    "Couldn't confirm this rule matches the conversation. Save anyway?",
                    &[("Save anyway", 'y'), ("Amend", 'a'), ("Cancel", 'c')],
                ) {
                    Some(0) => candidate,
                    Some(1) => {
                        let Some(amendment) = read_amendment(shell, "Amend TTSR rule") else {
                            return Ok(Step::Done);
                        };
                        feedback =
                            format!("User requested this amendment before saving:\n{amendment}\n");
                        previous = Some(candidate);
                        continue 'request;
                    }
                    _ => return Ok(Step::Done),
                }
            }
            (None, None) => {
                shell.note("OMFG", "The model did not return a valid TTSR rule.");
                return Ok(Step::Done);
            }
        };

        let (global, amendment) = match shell.ask(
            "Save TTSR rule where?",
            &candidate.name,
            &[
                ("This project (.dial/rules)", 'p'),
                ("Global — all projects", 'g'),
                ("Amend TTSR rule", 'a'),
                ("Cancel", 'c'),
            ],
        ) {
            Some(CHOICE_PROJECT) => (false, None),
            Some(CHOICE_GLOBAL) => (true, None),
            Some(CHOICE_AMEND) => match read_amendment(shell, "Amend TTSR rule") {
                Some(text) => (false, Some(text)),
                None => return Ok(Step::Done),
            },
            _ => return Ok(Step::Done),
        };
        if let Some(amendment) = amendment {
            feedback = format!("User requested this amendment before saving:\n{amendment}\n");
            previous = Some(candidate);
            continue 'request;
        }

        let target = agent.omfg_target(&candidate.name, global)?;
        let overwrite = if target.exists() {
            let body = format!("{} already exists. Overwrite it?", target.display());
            if shell.ask(
                "Overwrite TTSR rule?",
                &body,
                &[("Overwrite", 'o'), ("Cancel", 'c')],
            ) != Some(0)
            {
                return Ok(Step::Done);
            }
            true
        } else {
            false
        };

        match agent.omfg_save(&candidate, global, overwrite) {
            Ok(saved) => {
                let mut message = format!(
                    "Saved TTSR rule `{}` to {}.",
                    candidate.name,
                    saved.path.display()
                );
                if !saved.active {
                    message.push_str(
                        " Portable rules are disabled, so it loads once they are enabled.",
                    );
                }
                shell.note("OMFG", message);
            }
            Err(error) => shell.note("OMFG", format!("{error:#}")),
        }
        return Ok(Step::Done);
    }
}

/// Single-line editor for amendment text. Enter submits, Esc or Ctrl-C cancels.
fn read_amendment(shell: &mut Shell, title: &str) -> Option<String> {
    let mut editor = crate::ui::editor::Editor::default();
    let prompt = format!("{title} (Enter sends, Esc cancels)");
    let mut answer = None;
    loop {
        let (width, _) = shell.term.size();
        let theme = shell.themes.current();
        let rows = vec![
            Line::from(Span::styled(
                text::clip(&prompt, width as usize),
                theme.fg(Tok::Warning),
            )),
            Line::from(text::clip(&text::one_line(&editor.text), width as usize)),
        ];
        if shell.paint(Some(rows)).is_err() {
            break;
        }
        match read() {
            Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => match key.code {
                KeyCode::Enter => {
                    if !editor.text.trim().is_empty() {
                        answer = Some(editor.text);
                        break;
                    }
                }
                KeyCode::Esc => break,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                _ => {
                    editor.key(key);
                }
            },
            Ok(Event::Paste(value)) => editor.insert(&text::one_line(&value)),
            Ok(Event::Resize(..)) => shell.term.on_resize(),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    answer
}
