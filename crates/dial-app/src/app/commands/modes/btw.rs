//! `/btw <question>`: ask a side question without changing the transcript. The answer is shown as a note.

use crate::agent::Agent;
use crate::app::{App, Shell, Step};
use anyhow::Result;

pub(super) fn run(shell: &mut Shell, app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
    let question = args.trim();
    if question.is_empty() {
        shell.note("BTW", "usage: /btw <question>");
        return Ok(Step::Done);
    }
    if !shell.wait_compute(app)? {
        return Ok(Step::Done);
    }
    match agent.side_question(question) {
        Ok(answer) => shell.note("BTW", answer),
        Err(error) => shell.note("BTW", format!("error: {error:#}")),
    }
    Ok(Step::Done)
}
