//! `/tan <work>`: fork the session into a background clone that runs one request.
//! `/tan cancel <job-id>` aborts a running clone.

use crate::agent::Agent;
use crate::app::{App, Shell, Step};
use anyhow::Result;

pub(super) fn run(shell: &mut Shell, app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
    let args = args.trim();
    if args.is_empty() {
        shell.note("Tan", "Usage: /tan <work>");
        return Ok(Step::Done);
    }
    if args == "cancel" {
        shell.note("Tan", "Usage: /tan cancel <job-id>");
        return Ok(Step::Done);
    }
    if let Some(job_id) = args
        .strip_prefix("cancel ")
        .map(str::trim)
        .filter(|id| id.starts_with("tan-"))
    {
        let message = if agent.cancel_tan(job_id) {
            format!("Cancelled background tan {job_id}")
        } else {
            format!("No running background tan {job_id}")
        };
        shell.note("Tan", message);
        return Ok(Step::Done);
    }
    if !shell.wait_compute(app)? {
        return Ok(Step::Done);
    }
    match agent.start_tan(args) {
        Ok(job_id) => shell.note("Tan", format!("Dispatched background tan {job_id}")),
        Err(error) => shell.note("Tan", format!("{error:#}")),
    }
    Ok(Step::Done)
}
