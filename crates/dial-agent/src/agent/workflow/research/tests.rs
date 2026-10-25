use super::*;
use serde_json::json;

fn call(id: usize, name: &str) -> ToolCall {
    ToolCall {
        id: format!("read-{id}"),
        name: name.into(),
        arguments: json!({"path":format!("source-{id}.rs")}),
        malformed_arguments: false,
    }
}
fn observations(research: &mut Research, rounds: u32, batch: usize) {
    let start = research.rounds;
    for round in start..start + rounds {
        for index in 0..batch {
            research.observe(
                &call(round as usize * batch + index, "read"),
                &ToolOutput::ok(format!("new evidence {round}/{index}")),
                false,
            );
        }
        research.round();
    }
}
fn checkpoint(finding: &str, evidence: &str) -> Reply {
    Reply { text: json!({"finding":finding,"evidence":[evidence],"next_action":"Implement the supported correction, then test the failure boundary"}).to_string(), finish_reason:"stop".into(), ..Default::default() }
}

#[test]
fn distinct_successful_batched_lookups_require_synthesis_after_24_rounds() {
    let mut research = Research::default();
    observations(&mut research, WARN_ROUNDS - 1, 9);
    assert!(research.prompt().is_none());
    observations(&mut research, 1, 9);
    assert!(research.prompt().is_some());
    assert!(!research.required());
    observations(&mut research, SYNTHESIS_ROUNDS - WARN_ROUNDS, 9);
    assert!(research.required()); // 216 successful calls, all with different returned evidence.
    assert_eq!(research.rounds, 24);
    assert_eq!(research.evidence.len(), MAX_EVIDENCE);
}

#[test]
fn read_only_findings_renew_the_window_without_mutations_or_state_tools() {
    let mut research = Research::default();
    for window in 0..10 {
        observations(&mut research, SYNTHESIS_ROUNDS, 1);
        assert!(research.required());
        assert!(matches!(
            research.synthesize(&checkpoint(
                &format!("Established finding {window}"),
                "read-23"
            )),
            Synthesis::Accepted
        ));
        assert!(!research.required());
        assert!(research.prompt().is_none());
    }
}

#[test]
fn invalid_or_repeated_claims_cannot_renew_the_window() {
    let mut research = Research::default();
    observations(&mut research, SYNTHESIS_ROUNDS, 1);
    assert!(matches!(
        research.synthesize(&checkpoint(
            "The caller already batches requests",
            "read-23"
        )),
        Synthesis::Accepted
    ));
    observations(&mut research, SYNTHESIS_ROUNDS, 1);
    for reply in [
        checkpoint("New finding", "invented-id"),
        checkpoint(" THE caller already  batches requests \n", "read-22"),
        checkpoint("", "read-23"),
    ] {
        let result = research.synthesize(&reply);
        if research.attempts == MAX_ATTEMPTS {
            assert!(matches!(result, Synthesis::Incomplete(_)));
        } else {
            assert!(matches!(result, Synthesis::Retry(_)));
        }
        assert!(research.required());
    }
}

#[test]
fn checkpoint_tool_calls_and_truncated_or_inline_edits_never_get_accepted() {
    let mut research = Research::default();
    observations(&mut research, SYNTHESIS_ROUNDS, 1);
    let mut reply = checkpoint("New evidence", "read-23");
    reply.tool_calls.push(call(24, "write"));
    assert!(matches!(research.synthesize(&reply), Synthesis::Retry(_)));
    reply.tool_calls.clear();
    reply.finish_reason = "length".into();
    assert!(matches!(research.synthesize(&reply), Synthesis::Retry(_)));
    reply.finish_reason = "stop".into();
    reply.text = "*** Edit File: target.txt\n*** Find\nold\n*** Replace\nnew".into();
    assert!(matches!(
        research.synthesize(&reply),
        Synthesis::Incomplete(_)
    ));
}

#[test]
fn bookkeeping_failures_and_no_op_edits_do_not_reset_observations() {
    let mut research = Research::default();
    observations(&mut research, WARN_ROUNDS, 1);
    for name in ["todo", "working_state", "edit", "tool_load", "bg"] {
        research.observe(&call(100, name), &ToolOutput::ok("ok".into()), false);
        research.observe(&call(100, "read"), &ToolOutput::err("missing file"), false);
        research.round();
    }
    assert_eq!(research.rounds, WARN_ROUNDS);
    // A real milestone resets the window; other reads in the same batch do not count a round.
    research.observe(
        &call(101, "edit"),
        &ToolOutput::ok("committed".into()),
        true,
    );
    research.observe(
        &call(102, "read"),
        &ToolOutput::ok("verification input".into()),
        false,
    );
    research.round();
    assert_eq!(research.rounds, 0);
    observations(&mut research, SYNTHESIS_ROUNDS - 1, 1);
    assert!(!research.required());
}
