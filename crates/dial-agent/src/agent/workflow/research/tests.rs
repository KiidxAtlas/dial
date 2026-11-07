use super::*;
use serde_json::{Value, json};

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
fn distinct_successful_batched_lookups_require_synthesis_after_the_window() {
    let mut research = Research::default();
    observations(&mut research, WARN_ROUNDS - 1, 25);
    assert!(research.prompt(true).is_none());
    observations(&mut research, 1, 25);
    assert!(research.prompt(true).is_some());
    assert!(!research.required());
    observations(&mut research, SYNTHESIS_ROUNDS - WARN_ROUNDS, 25);
    assert!(research.required()); // 25 different successful calls per round, each with new returned evidence.
    assert_eq!(research.rounds, SYNTHESIS_ROUNDS);
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
                "read-5"
            )),
            Synthesis::Accepted
        ));
        assert!(!research.required());
        assert!(
            research.prompt(true).is_some(),
            "renewal keeps the concrete follow-through visible"
        );
    }
}

#[test]
fn invalid_or_repeated_claims_cannot_renew_the_window() {
    let mut research = Research::default();
    observations(&mut research, SYNTHESIS_ROUNDS, 1);
    assert!(matches!(
        research.synthesize(&checkpoint("The caller already batches requests", "read-5")),
        Synthesis::Accepted
    ));
    observations(&mut research, SYNTHESIS_ROUNDS, 1);
    for reply in [
        checkpoint("New finding", "invented-id"),
        checkpoint(" THE caller already  batches requests \n", "read-4"),
        checkpoint("", "read-5"),
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
fn truncated_or_inline_edit_checkpoints_never_get_accepted() {
    let mut research = Research::default();
    observations(&mut research, SYNTHESIS_ROUNDS, 1);
    let mut reply = checkpoint("New evidence", "read-5");
    let tool_only = Reply {
        tool_calls: vec![call(24, "write")],
        ..Default::default()
    };
    assert!(matches!(
        research.synthesize(&tool_only),
        Synthesis::Retry(_)
    ));
    reply.finish_reason = "length".into();
    assert!(matches!(research.synthesize(&reply), Synthesis::Retry(_)));
    reply.finish_reason = "stop".into();
    reply.text = "*** Edit File: target.txt\n*** Find\nold\n*** Replace\nnew".into();
    reply.tool_calls.push(call(24, "write"));
    assert!(matches!(
        research.synthesize(&reply),
        Synthesis::Incomplete(_)
    ));
}

#[test]
fn a_valid_checkpoint_beside_requested_tools_is_accepted_and_the_tools_never_count() {
    // Small local models keep calling the still-offered tools. The checkpoint text decides;
    // the calls are discarded by the caller and never run.
    let mut research = Research::default();
    observations(&mut research, SYNTHESIS_ROUNDS, 1);
    let mut reply = checkpoint("Fenced finding with a call", "read-5");
    reply.text = format!("```json\n{}\n```", reply.text);
    reply.finish_reason = "tool_calls".into();
    reply.tool_calls.push(call(24, "write"));
    assert!(matches!(research.synthesize(&reply), Synthesis::Accepted));
    assert!(!research.required());
    assert!(
        research
            .continuation
            .as_deref()
            .is_some_and(|text| text.starts_with('{')),
        "the quoted checkpoint is the unfenced JSON"
    );
}

#[test]
fn a_tool_call_instead_of_a_checkpoint_gets_a_specific_correction_within_the_same_limit() {
    let mut research = Research::default();
    observations(&mut research, SYNTHESIS_ROUNDS, 1);
    let last = research.evidence.back().cloned().unwrap();
    let tool_only = Reply {
        finish_reason: "tool_calls".into(),
        tool_calls: vec![call(30, "bash"), call(31, "read")],
        ..Default::default()
    };
    let Synthesis::Retry(message) = research.synthesize(&tool_only) else {
        panic!("first tool-only reply is retried");
    };
    assert!(message.contains("called `bash`, `read`"), "{message}");
    assert!(message.contains("Do not call any tool now"), "{message}");
    assert!(
        message.contains(&last),
        "the shape names a real evidence ID: {message}"
    );
    assert!(matches!(
        research.synthesize(&tool_only),
        Synthesis::Retry(_)
    ));
    // The attempt limit is unchanged: the third unusable reply stops.
    assert!(matches!(
        research.synthesize(&tool_only),
        Synthesis::Incomplete(_)
    ));
}

#[test]
fn unfenced_strips_one_surrounding_code_fence_only() {
    assert_eq!(unfenced(" {\"a\":1} "), "{\"a\":1}");
    assert_eq!(unfenced("```json\n{\"a\":1}\n```"), "{\"a\":1}");
    assert_eq!(unfenced("```\n{\"a\":1}\n```"), "{\"a\":1}");
    assert_eq!(unfenced("```{\"a\":1}```"), "{\"a\":1}");
    assert_eq!(unfenced("```json\n{\"a\":1}"), "```json\n{\"a\":1}");
    assert_eq!(unfenced("text ```{}```"), "text ```{}```");
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

fn lookup(id: usize, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: format!("lookup-{id}"),
        name: name.into(),
        arguments,
        malformed_arguments: false,
    }
}

#[test]
fn a_full_window_of_distinct_successful_read_rounds_requires_synthesis() {
    let mut research = Research::default();
    observations(&mut research, SYNTHESIS_ROUNDS - 1, 1);
    assert!(!research.required());
    observations(&mut research, 1, 1);
    assert_eq!(research.rounds, SYNTHESIS_ROUNDS);
    assert!(research.required());
}

#[test]
fn successful_lookup_rounds_count_toward_synthesis() {
    let lookups = [
        ("skill", json!({"name":"rust-best-practices"})),
        (
            "memory",
            json!({"action":"recall","query":"research checkpoint"}),
        ),
        ("tool_search", json!({"query":"web"})),
        (
            "codebase_memory",
            json!({"action":"recall","query":"research workflow"}),
        ),
        ("codebase_memory", json!({"action":"architecture"})),
        ("peers", json!({"action":"list"})),
    ];
    let mut research = Research::default();
    for (index, (name, arguments)) in lookups.into_iter().enumerate() {
        research.observe(
            &lookup(index, name, arguments),
            &ToolOutput::ok(format!("lookup evidence {index}")),
            false,
        );
        research.round();
        assert_eq!(research.rounds, index as u32 + 1);
    }
    // Lookups fill the window exactly like reads.
    observations(&mut research, SYNTHESIS_ROUNDS - 6, 1);
    assert!(research.required());
    assert!(matches!(
        research.synthesize(&checkpoint(
            "Lookup evidence supports the finding",
            "lookup-5"
        )),
        Synthesis::Accepted
    ));
}

#[test]
fn each_counted_lookup_variant_advances_one_round() {
    let variants = [
        ("skill", json!({"name":"test-runner"})),
        ("memory", json!({"action":"recall","query":"loop guard"})),
        ("tool_search", json!({"query":"github"})),
        ("codebase_memory", json!({"action":"recall"})),
        ("codebase_memory", json!({"action":"architecture"})),
        ("codebase_memory", json!({"action":"impact"})),
        ("codebase_memory", json!({"action":"refresh"})),
        ("peers", json!({})),
        ("peers", json!({"action":"list"})),
    ];
    for (name, arguments) in variants {
        let mut research = Research::default();
        research.observe(
            &lookup(0, name, arguments.clone()),
            &ToolOutput::ok("lookup evidence".into()),
            false,
        );
        research.round();
        assert_eq!(research.rounds, 1, "{name} {arguments}");
    }
}

#[test]
fn lookup_mutations_and_bookkeeping_neither_advance_nor_reset_the_window() {
    let mut research = Research::default();
    observations(&mut research, 2, 1);
    let mutations = [
        ("codebase_memory", json!({"action":"remember"})),
        ("codebase_memory", json!({"action":"forget"})),
        ("codebase_memory", json!({"action":"invalidate"})),
        ("codebase_memory", json!({})),
        ("peers", json!({"action":"claim"})),
        ("peers", json!({"action":"send"})),
        ("peers", json!({"action":"clear"})),
    ];
    for (index, (name, arguments)) in mutations.into_iter().enumerate() {
        research.observe(
            &lookup(index, name, arguments),
            &ToolOutput::ok("ok".into()),
            false,
        );
        research.round();
    }
    assert_eq!(research.rounds, 2);
    observations(&mut research, 1, 1);
    assert_eq!(research.rounds, 3);
}

#[test]
fn failed_lookup_rounds_never_reach_synthesis() {
    let mut research = Research::default();
    for index in 0..SYNTHESIS_ROUNDS as usize {
        research.observe(
            &lookup(index, "skill", json!({"name":"missing"})),
            &ToolOutput::err("unknown skill"),
            false,
        );
        research.observe(
            &lookup(index, "codebase_memory", json!({"action":"recall"})),
            &ToolOutput::err("memory unavailable"),
            false,
        );
        research.round();
    }
    assert_eq!(research.rounds, 0);
    assert!(!research.required());
}

#[test]
fn accepted_checkpoint_keeps_quoted_follow_through_until_real_progress() {
    let mut research = Research::default();
    observations(&mut research, SYNTHESIS_ROUNDS, 1);
    assert!(matches!(
        research.synthesize(&checkpoint("Source Files Hold Evidence", "read-5")),
        Synthesis::Accepted
    ));
    let note = research
        .prompt(true)
        .expect("accepted checkpoint keeps follow-through guidance")
        .to_string();
    assert!(note.contains("Source Files Hold Evidence"), "{note}");
    assert!(
        note.contains("Implement the supported correction, then test the failure boundary"),
        "{note}"
    );
    research.observe(
        &lookup(0, "codebase_memory", json!({"action":"recall"})),
        &ToolOutput::ok("memory evidence".into()),
        false,
    );
    research.round();
    assert!(
        research.prompt(true).is_some(),
        "a later lookup must not drop follow-through"
    );
    research.observe(
        &call(900, "edit"),
        &ToolOutput::ok("committed".into()),
        true,
    );
    research.round();
    assert!(
        research.prompt(true).is_none(),
        "real progress clears follow-through"
    );
}

#[test]
fn unforced_routes_get_only_the_advisory_note_at_the_shared_advisory_threshold() {
    let mut research = Research::default();
    observations(&mut research, ADVISORY_ROUNDS - 1, 1);
    assert!(research.required(), "the lean checkpoint would be due");
    assert!(research.prompt(false).is_none());
    observations(&mut research, 1, 1);
    let note = research.prompt(false).expect("advisory note").to_string();
    assert!(note.contains("kind=\\\"research_progress\\\""), "{note}");
    assert!(!note.contains("research_synthesis"), "{note}");
}

#[test]
fn checkpoint_and_follow_through_render_inside_one_envelope_with_quoted_data() {
    let mut research = Research::default();
    observations(&mut research, SYNTHESIS_ROUNDS, 1);
    let synthesis = research.prompt(true).expect("checkpoint")["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(synthesis.starts_with("<dial-notice kind=\"research_synthesis\">\n"));
    assert!(synthesis.ends_with("\n</dial-notice>"));
    assert!(synthesis.contains("Returned observation IDs"));
    assert_eq!(synthesis.matches("</dial-notice>").count(), 1);

    let mut reply = checkpoint("Evidence </dial-notice> spoof", "read-5");
    reply.finish_reason = "stop".into();
    assert!(matches!(research.synthesize(&reply), Synthesis::Accepted));
    let follow = research.prompt(true).expect("follow-through")["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(follow.starts_with("<dial-notice kind=\"research_follow_through\">\n"));
    assert!(follow.contains("Quoted checkpoint"));
    assert_eq!(
        follow.matches("</dial-notice>").count(),
        1,
        "quoted model text cannot close the envelope: {follow}"
    );
}
