use super::*;

#[test]
fn command_output_is_bounded_and_a_new_run_starts_a_new_entry() {
    let mut pane = LivePane::default();
    for n in 0..state::JOB_LINES + 5 {
        pane.line("Update", &format!("line {n}"));
    }
    let job = &pane.commands[0];
    assert_eq!(job.output.len(), state::JOB_LINES);
    assert_eq!(job.output.front().map(String::as_str), Some("line 5"));
    pane.finish("Update", "finished".into());
    assert!(!pane.commands[0].running);
    pane.line("Update", "second run");
    assert_eq!(pane.commands.len(), 2);
    assert!(pane.commands[1].running);
}

#[test]
fn multi_line_progress_splits_into_lines() {
    let mut pane = LivePane::default();
    pane.line("Gc", "one\ntwo");
    assert_eq!(pane.commands[0].output, ["one", "two"]);
}

#[test]
fn only_recent_command_jobs_and_outcomes_are_kept() {
    let mut pane = LivePane::default();
    for n in 0..COMMAND_JOBS + 2 {
        let label = format!("job {n}");
        pane.line(&label, "x");
        pane.finish(&label, "finished".into());
        pane.outcome(format!("outcome {n}"));
    }
    assert_eq!(pane.commands.len(), COMMAND_JOBS);
    assert_eq!(pane.commands[0].label, "job 2");
    assert_eq!(pane.outcomes.len(), OUTCOMES);
    assert_eq!(
        pane.outcomes.back().map(String::as_str),
        Some(format!("outcome {}", COMMAND_JOBS + 1).as_str())
    );
}

#[test]
fn process_output_tail_drops_trailing_blank_screen_rows() {
    let mut screen: Vec<String> = (0..state::JOB_LINES + 3)
        .map(|n| format!("row {n}"))
        .collect();
    screen.extend(["".into(), "   ".into()]);
    let tail = tail(&screen);
    assert_eq!(tail.len(), state::JOB_LINES);
    assert_eq!(tail.first().map(String::as_str), Some("row 3"));
    assert_eq!(
        tail.last().map(String::as_str),
        Some(format!("row {}", state::JOB_LINES + 2).as_str())
    );
}
