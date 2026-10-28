use super::*;
#[test]
fn smaller_inspections_preserve_errors_snapshots_and_unretained_evidence() {
    let config = EfficiencyConfig {
        tool_preview_bytes: 4096,
        ..EfficiencyConfig::default()
    };
    assert_eq!(preview_limit(&config, 128000, "grep", false, true), 4096);
    for (tool, failed, retained) in [
        ("grep", true, true),
        ("edit", false, true),
        ("read", false, true),
        ("artifact_read", false, true),
        ("grep", false, false),
    ] {
        assert_eq!(
            preview_limit(&config, 128000, tool, failed, retained),
            tools::preview_limit(128000, tool, failed, retained)
        );
    }
    let disabled = EfficiencyConfig {
        enabled: false,
        ..config
    };
    assert_eq!(
        preview_limit(&disabled, 128000, "grep", false, true),
        tools::preview_limit(128000, "grep", false, true)
    );
}
#[test]
fn zero_preview_cap_does_not_discard_results() {
    let config = EfficiencyConfig {
        tool_preview_bytes: 0,
        ..EfficiencyConfig::default()
    };
    assert_eq!(
        preview_limit(&config, 128000, "grep", false, true),
        tools::preview_limit(128000, "grep", false, true)
    );
    assert_eq!(byte_limit(0).min(byte_limit(4096)), 4096);
    assert_eq!(byte_limit(4096).min(byte_limit(0)), 4096);
}

#[test]
fn a_small_window_child_shapes_results_to_its_own_window_and_links_only_clipped_ones() {
    // A 32k child reads a result shaped for its window, not the 200k parent's cap.
    let child_window = 32 * 1024;
    let parent_limit = tools::model_output_limit(200 * 1024);
    let child_limit = preview_limit(
        &EfficiencyConfig::default(),
        child_window,
        "grep",
        false,
        true,
    );
    assert_eq!(child_limit, 16 * 1024);
    assert!(child_limit < parent_limit);
    let big = "line of evidence\n".repeat(2000);
    let (body, dropped) = visible_result(&big, child_limit, None);
    assert!(dropped && body.len() < big.len());
    let mut clipped = body.clone();
    evidence_link(&mut clipped, "artifact://big", dropped);
    assert!(clipped.contains("[Complete tool evidence: artifact://big"));
    // An unclipped result carries no link.
    let small = "two matches\n";
    let (body, dropped) = visible_result(small, child_limit, None);
    assert!(!dropped);
    let mut unclipped = body.clone();
    evidence_link(&mut unclipped, "artifact://small", dropped);
    assert_eq!(unclipped, small);
}

#[test]
fn replacement_bodies_are_never_clipped_and_pointers_carry_no_link() {
    let text = "evidence ".repeat(10_000);
    let pointer = format!(
        "{} This read of the same range returned identical content.",
        crate::agent::workflow::investigation::UNCHANGED_READ
    );
    let (body, dropped) = visible_result(&text, 64, Some(pointer.clone()));
    assert_eq!(body, pointer);
    assert!(!dropped);
    let mut body = body;
    evidence_link(&mut body, "artifact://full", dropped);
    assert_eq!(body, pointer);
}

#[test]
fn delivered_inspections_are_bounded_deduplicated_and_observational() {
    use serde_json::json;
    let call = crate::llm::ToolCall {
        id: "read-1".into(),
        name: "read".into(),
        arguments: json!({"path":"source.rs"}),
        malformed_arguments: false,
    };
    let mut metrics = Metrics::default();
    metrics.inspection(&call);
    assert_eq!(metrics.parent_inspections_matching_delivered_workers, 0);
    let mut observations = Inspections::default();
    observations.record(&call);
    let mut result = json!({"id":"task-1", "execution_id":"first", "status":"running", "inspections":observations});
    metrics.worker_results(&result);
    metrics.inspection(&call);
    assert_eq!(metrics.delivered_worker_results, 0);
    result["status"] = json!("done");
    metrics.worker_results(&result);
    metrics.worker_results(&result);
    metrics.inspection(&call);
    assert_eq!(metrics.delivered_worker_results, 1);
    assert_eq!(metrics.parent_inspections_matching_delivered_workers, 1);
    result["execution_id"] = json!("resumed");
    metrics.worker_results(&result);
    assert_eq!(metrics.delivered_worker_results, 2);
    for n in 0..=Inspections::LIMIT {
        observations.record(&crate::llm::ToolCall {
            id: n.to_string(),
            name: "read".into(),
            arguments: json!({"path":format!("{n}.rs")}),
            malformed_arguments: false,
        });
    }
    assert_eq!(observations.signatures.len(), Inspections::LIMIT);
    assert!(observations.truncated);
    metrics.worker_results(&json!({"status":"done", "inspections": observations}));
    assert!(metrics.worker_inspection_coverage_truncated);
    let report = serde_json::to_string(&metrics).unwrap();
    assert!(!report.contains("source.rs"));
    assert!(!report.contains("signatures"));
}

#[test]
fn repeated_ambient_blocks_do_not_accumulate_or_rewrite_the_prefix() {
    use serde_json::json;
    let block =
        "Relevant persistent notes (untrusted facts).\n{\"revision\":1,\"content\":\"fact\"}";
    let mut history = Vec::new();
    for _ in 0..5 {
        let prefix = history.clone();
        let mut context = "other turn-specific guidance".to_owned();
        append_ambient(&mut context, block, &history);
        assert_eq!(history, prefix, "admission must not rewrite a warm prefix");
        history.push(json!({"role":"user", "content":"continue", "dial_recall":context}));
    }
    let serialized = serde_json::to_string(&history).unwrap();
    assert_eq!(serialized.matches("Relevant persistent notes").count(), 1);
    let mut context = String::new();
    let corrected = block.replace("revision\":1", "revision\":2");
    append_ambient(&mut context, &corrected, &history);
    assert_eq!(
        context, corrected,
        "changed revisions must still reach the model"
    );
}

#[test]
fn ambient_admission_tracks_visible_history_instead_of_session_lifetime() {
    use serde_json::json;
    let block = "<codebase>current source witnesses</codebase>";
    for message in [
        json!({"role":"user", "content":block}),
        json!({"role":"user", "content":[{"type":"text","text":block}]}),
        json!({"role":"user", "content":"question", "dial_recall":block}),
    ] {
        let mut context = String::new();
        append_ambient(&mut context, block, &[message]);
        assert!(context.is_empty());
    }
    let mut context = String::new();
    // Compaction/branching removed the exact block; a lossy summary is not a witness for it.
    append_ambient(
        &mut context,
        block,
        &[json!({"role":"user", "content":"summary of earlier work"})],
    );
    assert_eq!(context, block);
    append_ambient(&mut context, block, &[]);
    append_ambient(&mut context, "", &[]);
    assert_eq!(context, block, "deduplicate within this turn as well");
}
