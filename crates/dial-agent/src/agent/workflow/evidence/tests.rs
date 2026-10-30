use super::*;
#[test]
fn preserves_mutations_and_replaces_only_superseded_read() {
    let mut m = vec![
        obs("1", &"old".repeat(100), "a:1-5"),
        json!({"role":"tool","tool_call_id":"2","content":"changed file"}),
        obs("3", "new", "a:1-5"),
    ];
    project(&mut m, &mut Supersession::default(), &Policy::calm());
    assert!(m[0]["content"].as_str().unwrap().contains("Superseded"));
    assert_eq!(m[1]["content"], "changed file");
    assert_eq!(m[2]["content"], "new");
}
fn obs(id: &str, text: &str, key: &str) -> Value {
    json!({"role":"tool","tool_call_id":id,"content":text,"dial_evidence":{"observation":key,"artifact":format!("artifact://{id}")}})
}
#[test]
fn a_placeholder_never_changes_after_it_is_written() {
    // A moving "newer result at message N" pointer would rewrite every older placeholder on each re-read and
    // invalidate the provider's prompt-prefix cache from the oldest one onward.
    let mut memo = Supersession::default();
    let mut first = vec![
        obs("1", &"v1".repeat(100), "k"),
        obs("2", &"v2".repeat(100), "k"),
    ];
    project(&mut first, &mut memo, &Policy::calm());
    let mut later = vec![
        obs("1", &"v1".repeat(100), "k"),
        obs("2", &"v2".repeat(100), "k"),
        json!({"role":"assistant","content":"x"}),
        obs("3", "v3", "k"),
    ];
    project(&mut later, &mut memo, &Policy::calm());
    assert_eq!(first[0]["content"], later[0]["content"]);
    assert!(
        later[1]["content"]
            .as_str()
            .unwrap()
            .starts_with("[Superseded")
    );
}
#[test]
fn a_read_far_behind_a_large_warm_suffix_stays_until_context_pressure() {
    let big = "x".repeat(40_000);
    let conversation = || {
        vec![
            obs("1", &"old".repeat(100), "k"),
            json!({"role":"assistant","content":big.clone()}),
            obs("2", "new", "k"),
        ]
    };
    let mut memo = Supersession::default();
    let policy = |pressure| Policy {
        warm_tokens: 1_000,
        pressure,
        protect_tokens: usize::MAX,
        supersede_reads: true,
        drop_useless: true,
    };
    let mut calm = conversation();
    project(&mut calm, &mut memo, &policy(false));
    assert_eq!(
        calm[0]["content"],
        "old".repeat(100),
        "superseding would invalidate ~10k cached tokens for little saving"
    );
    let mut pressed = conversation();
    project(&mut pressed, &mut memo, &policy(true));
    assert!(
        pressed[0]["content"]
            .as_str()
            .unwrap()
            .contains("Superseded")
    );
    // And once superseded it must not flip back when pressure eases.
    let mut eased = conversation();
    project(&mut eased, &mut memo, &policy(false));
    assert!(eased[0]["content"].as_str().unwrap().contains("Superseded"));
}
#[test]
fn recall_is_folded_into_its_own_user_message_not_inserted_before_the_conversation() {
    let mut m = vec![
        json!({"role":"user","content":"question","dial_recall":"NOTE: fact"}),
        json!({"role":"assistant","content":"a"}),
        json!({"role":"user","content":"later"}),
    ];
    project(&mut m, &mut Supersession::default(), &Policy::calm());
    assert_eq!(m.len(), 3);
    assert_eq!(m[0]["content"], "question\n\nNOTE: fact");
    assert!(m[0].get("dial_recall").is_none());
    assert_eq!(m[2]["content"], "later");
}
fn exchange(id: &str, tool: &str, size: usize) -> Vec<Value> {
    vec![
        json!({"role":"assistant","content":"","tool_calls":[{"id":id,"function":{"name":tool,"arguments":"{}"}}]}),
        json!({"role":"tool","tool_call_id":id,"content":"y".repeat(size),"dial_evidence":{"artifact":format!("artifact://{id}")}}),
    ]
}
fn history() -> Vec<Value> {
    let mut m = vec![json!({"role":"user","content":"task"})];
    for (id, tool, size) in [
        // 10k estimated tokens: on its own enough to clear the prune batch threshold.
        ("old-grep", "grep", 40_000),
        ("old-edit", "edit", 20_000),
        ("old-small", "grep", 400),
        ("old-bash", "bash", 20_000),
        ("new-read", "read", 20_000),
    ] {
        m.extend(exchange(id, tool, size));
    }
    m
}
fn content<'a>(m: &'a [Value], id: &str) -> &'a str {
    m.iter().find(|x| x["tool_call_id"] == id).unwrap()["content"]
        .as_str()
        .unwrap()
}
#[test]
fn pressure_drops_old_large_observations_but_never_edits_small_results_or_the_recent_tail() {
    let mut m = history();
    let policy = Policy {
        warm_tokens: 0,
        pressure: true,
        protect_tokens: 4_000,
        supersede_reads: true,
        drop_useless: true,
    };
    project(&mut m, &mut Supersession::default(), &policy);
    assert!(
        content(&m, "old-grep").starts_with("[Older grep result omitted")
            && content(&m, "old-grep").contains("artifact://old-grep")
            && content(&m, "old-grep").contains("40000 bytes")
    );
    assert_eq!(
        content(&m, "old-bash").len(),
        20_000,
        "shell output may be mutation or verification evidence"
    );
    assert_eq!(
        content(&m, "old-edit").len(),
        20_000,
        "a mutation's evidence stays verbatim"
    );
    assert_eq!(
        content(&m, "old-small").len(),
        400,
        "too small to be worth a placeholder"
    );
    assert_eq!(
        content(&m, "new-read").len(),
        20_000,
        "the newest results are inside the protected tail"
    );
}
#[test]
fn pressure_preserves_error_diagnostics_and_artifactless_results() {
    let mut m = exchange("failed", "grep", 20_000);
    m[1]["dial_evidence"]["is_error"] = json!(true);
    m.extend(exchange("unretained", "read", 20_000));
    m[3].as_object_mut().unwrap().remove("dial_evidence");
    m.push(json!({"role":"user","content":"tail"}));
    project(
        &mut m,
        &mut Supersession::default(),
        &Policy {
            warm_tokens: 0,
            pressure: true,
            protect_tokens: 0,
            supersede_reads: true,
            drop_useless: true,
        },
    );
    assert_eq!(content(&m, "failed").len(), 20_000);
    assert_eq!(content(&m, "unretained").len(), 20_000);
}

#[test]
fn pressure_drops_old_bounded_investigation_results_but_keeps_stateful_ones() {
    let mut m = vec![json!({"role":"user","content":"task"})];
    for (id, tool) in [
        ("old-data", "data"),
        ("old-trace", "runtime_inspect"),
        ("old-research", "research"),
    ] {
        m.extend(exchange(id, tool, 40_000));
    }
    m.extend(exchange("new-read", "read", 20_000));
    project(
        &mut m,
        &mut Supersession::default(),
        &Policy {
            warm_tokens: 0,
            pressure: true,
            protect_tokens: 4_000,
            supersede_reads: true,
            drop_useless: true,
        },
    );
    assert!(content(&m, "old-data").starts_with("[Older data result omitted"));
    assert!(content(&m, "old-trace").starts_with("[Older runtime_inspect result omitted"));
    assert_eq!(
        content(&m, "old-research").len(),
        40_000,
        "tools that record state keep their results verbatim"
    );
}

#[test]
fn nothing_is_dropped_without_pressure() {
    let mut m = history();
    project(
        &mut m,
        &mut Supersession::default(),
        &Policy {
            warm_tokens: 0,
            pressure: false,
            protect_tokens: 4_000,
            supersede_reads: true,
            drop_useless: true,
        },
    );
    assert!(
        m.iter()
            .filter(|x| x["role"] == "tool")
            .all(|x| !x["content"].as_str().unwrap().starts_with("[Older"))
    );
}
#[test]
fn a_dropped_result_stays_dropped_and_byte_identical_after_pressure_eases() {
    let mut memo = Supersession::default();
    let mut pressed = history();
    project(
        &mut pressed,
        &mut memo,
        &Policy {
            warm_tokens: 0,
            pressure: true,
            protect_tokens: 4_000,
            supersede_reads: true,
            drop_useless: true,
        },
    );
    let mut eased = history();
    project(
        &mut eased,
        &mut memo,
        &Policy {
            warm_tokens: 0,
            pressure: false,
            protect_tokens: 4_000,
            supersede_reads: true,
            drop_useless: true,
        },
    );
    assert_eq!(
        content(&pressed, "old-grep"),
        content(&eased, "old-grep"),
        "flipping back would rewrite the cached prefix"
    );
}
#[test]
fn small_prunes_wait_until_together_they_free_a_whole_batch() {
    // Each candidate is 5k estimated tokens, below PRUNE_BATCH_MIN_TOKENS. Pruning one per step
    // as it crossed the protected tail would rewrite the cached prefix on nearly every step.
    let policy = Policy {
        warm_tokens: 0,
        pressure: true,
        protect_tokens: 4_000,
        supersede_reads: true,
        drop_useless: true,
    };
    let mut memo = Supersession::default();
    let mut m = vec![json!({"role":"user","content":"task"})];
    m.extend(exchange("a", "grep", 20_000));
    m.extend(exchange("tail", "read", 20_000));
    let mut first = m.clone();
    project(&mut first, &mut memo, &policy);
    assert_eq!(
        content(&first, "a").len(),
        20_000,
        "one 5k candidate is held until the batch is worth a cache break"
    );

    let mut m = vec![json!({"role":"user","content":"task"})];
    m.extend(exchange("a", "grep", 20_000));
    m.extend(exchange("b", "grep", 20_000));
    m.extend(exchange("tail", "read", 20_000));
    project(&mut m, &mut memo, &policy);
    assert!(content(&m, "a").starts_with("[Older grep result omitted"));
    assert!(content(&m, "b").starts_with("[Older grep result omitted"));
    assert_eq!(
        content(&m, "tail").len(),
        20_000,
        "the protected tail is never part of a batch"
    );
}
#[test]
fn a_dropped_read_that_is_later_repeated_keeps_its_first_placeholder() {
    let mut memo = Supersession::default();
    let mut m = vec![json!({"role":"user","content":"task"})];
    // Large enough (10k estimated tokens) to be pruned as its own batch.
    let mut first = exchange("r1", "read", 40_000);
    first[1]["dial_evidence"]["observation"] = json!("k");
    m.extend(first);
    m.extend(exchange("pad", "grep", 30_000));
    project(
        &mut m.clone(),
        &mut memo,
        &Policy {
            warm_tokens: 0,
            pressure: true,
            protect_tokens: 6_000,
            supersede_reads: true,
            drop_useless: true,
        },
    );
    let before = m.clone();
    let mut projected = before.clone();
    project(
        &mut projected,
        &mut memo,
        &Policy {
            warm_tokens: 0,
            pressure: false,
            protect_tokens: 6_000,
            supersede_reads: true,
            drop_useless: true,
        },
    );
    let first_placeholder = content(&projected, "r1").to_owned();
    assert!(first_placeholder.starts_with("[Older read result omitted"));
    let mut again = before;
    let mut repeat = exchange("r2", "read", 20_000);
    repeat[1]["dial_evidence"]["observation"] = json!("k");
    again.extend(repeat);
    project(
        &mut again,
        &mut memo,
        &Policy {
            warm_tokens: 0,
            pressure: false,
            protect_tokens: 6_000,
            supersede_reads: true,
            drop_useless: true,
        },
    );
    assert_eq!(
        content(&again, "r1"),
        first_placeholder,
        "the repeat must not turn the placeholder into a different one"
    );
}
#[test]
fn default_pressure_reduces_only_superseded_observations_with_before_after_sizes() {
    let mut m = vec![json!({"role":"user","content":"task"})];
    let mut err = exchange("err", "read", 40_000);
    err[1]["dial_evidence"]["observation"] = json!("k");
    err[1]["dial_evidence"]["is_error"] = json!(true);
    m.extend(err);
    let mut stale = exchange("stale", "read", 40_000);
    stale[1]["dial_evidence"]["observation"] = json!("k");
    m.extend(stale);
    m.extend(exchange("edit", "edit", 20_000));
    let mut latest = exchange("latest", "read", 40_000);
    latest[1]["dial_evidence"]["observation"] = json!("k");
    m.extend(latest);
    m.extend(exchange("tail", "grep", 20_000));
    fn bytes(m: &[Value]) -> usize {
        m.iter()
            .map(|x| x["content"].as_str().map_or(0, str::len))
            .sum()
    }
    let policy = |pressure| Policy {
        warm_tokens: 0,
        pressure,
        protect_tokens: 4_000,
        supersede_reads: false,
        drop_useless: false,
    };
    let before = bytes(&m);
    let mut calm = m.clone();
    project(&mut calm, &mut Supersession::default(), &policy(false));
    assert_eq!(bytes(&calm), before, "calm requests keep every result");
    project(&mut m, &mut Supersession::default(), &policy(true));
    let after = bytes(&m);
    eprintln!("superseded observations under pressure: {before} -> {after} bytes");
    assert!(after < before);
    assert!(content(&m, "stale").starts_with("[Older read result omitted"));
    assert!(content(&m, "stale").contains("artifact://stale"));
    assert_eq!(
        content(&m, "latest").len(),
        40_000,
        "the newest copy stays in full"
    );
    assert_eq!(content(&m, "err").len(), 40_000, "failures stay verbatim");
    assert_eq!(content(&m, "edit").len(), 20_000, "mutations stay verbatim");
    assert_eq!(
        content(&m, "tail").len(),
        20_000,
        "the protected tail stays verbatim"
    );
}
#[test]
fn repeated_errors_never_supersede_success_or_lose_diagnostics() {
    for failed in [0, 1] {
        let mut messages = exchange("old", "read", 40_000);
        messages.extend(exchange("new", "read", 20_000));
        messages[1]["dial_evidence"]["observation"] = json!("same-range");
        messages[3]["dial_evidence"]["observation"] = json!("same-range");
        messages[1 + failed * 2]["dial_evidence"]["is_error"] = json!(true);
        let before = messages.clone();
        project(
            &mut messages,
            &mut Supersession::default(),
            &Policy {
                pressure: true,
                protect_tokens: 0,
                warm_tokens: 0,
                supersede_reads: true,
                drop_useless: false,
            },
        );
        for id in ["old", "new"] {
            assert!(
                content(&messages, id) == content(&before, id),
                "{id} diagnostics changed"
            );
        }
    }
}

#[test]
fn structured_or_image_suffixes_do_not_look_like_an_empty_warm_prefix() {
    for suffix in [
        json!({"role":"user","content":[{"type":"text","text":"x".repeat(40_000)}]}),
        json!({"role":"user","content":"inspect this", "dial_images":[{"path":"fixture.png"}]}),
        json!({"role":"assistant","content":"", "dial_provider_content":[{"type":"thinking","thinking":"x".repeat(40_000)}]}),
    ] {
        let mut messages = vec![
            obs("old", "original source", "k"),
            suffix,
            obs("new", "updated source", "k"),
        ];
        project(
            &mut messages,
            &mut Supersession::default(),
            &Policy {
                warm_tokens: 1_000,
                pressure: false,
                protect_tokens: usize::MAX,
                supersede_reads: true,
                drop_useless: true,
            },
        );
        assert_eq!(
            messages[0]["content"], "original source",
            "Do not invalidate a large cached suffix outside pressure"
        );
    }
}

#[test]
fn exact_successful_search_repeats_shrink_without_touching_history_or_pairing() {
    let mut original = exchange("old", "grep", 8_000);
    original[1]["dial_evidence"]["is_error"] = json!(false);
    original[1]["dial_file_handles"] = json!({"f7":"src/main.rs"});
    original.push(json!({"role":"user","content":"Keep these active instructions"}));
    original.extend(exchange("new", "grep", 8_000));
    original[4]["dial_evidence"]["is_error"] = json!(false);
    let history = original.clone();
    let mut memo = Supersession::default();
    project(&mut original, &mut memo, &Policy::calm());
    assert!(content(&original, "old").starts_with("[Superseded observation"));
    assert_eq!(content(&original, "new").len(), 8_000);
    assert!(
        !memo.holds_full("old"),
        "omitted reads cannot supply edit witnesses"
    );
    assert!(memo.holds_full("new"));
    assert_eq!(original.len(), history.len());
    for i in [0, 2, 3, 4] {
        assert_eq!(
            original[i],
            crate::llm::messages::canonical(&history[i]),
            "nonredundant message {i}"
        );
    }
    assert_eq!(original[1]["tool_call_id"], history[1]["tool_call_id"]);
    assert!(
        original[1].get("dial_file_handles").is_none(),
        "private mappings must not leak to the model"
    );
    assert_eq!(history[1]["dial_file_handles"], json!({"f7":"src/main.rs"}));
    assert_eq!(
        content(&history, "old").len(),
        8_000,
        "stored evidence remains full"
    );
    let mut later = history.clone();
    later.push(json!({"role":"assistant","content":"x".repeat(60_000)}));
    project(
        &mut later,
        &mut memo,
        &Policy {
            warm_tokens: 0,
            ..Policy::calm()
        },
    );
    assert_eq!(
        original[1], later[1],
        "memoized replacement stays byte-identical"
    );
}

#[test]
fn exact_repeats_require_the_same_call_payload_and_positive_success() {
    for case in [
        "arguments",
        "content",
        "error",
        "unknown",
        "missing_status",
        "image",
        "provider_content",
        "missing_artifact",
        "mutation",
        "unpaired",
    ] {
        let tool = if case == "mutation" { "edit" } else { "grep" };
        let mut messages = exchange("old", tool, 8_000);
        messages.extend(exchange("new", tool, 8_000));
        messages[1]["dial_evidence"]["is_error"] = json!(false);
        messages[3]["dial_evidence"]["is_error"] = json!(false);
        match case {
            "arguments" => {
                messages[2]["tool_calls"][0]["function"]["arguments"] =
                    json!("{\"pattern\":\"different\"}")
            }
            "content" => messages[3]["content"] = json!("different output"),
            "error" => messages[3]["dial_evidence"]["is_error"] = json!(true),
            "unknown" => messages[3]["dial_effects"] = json!("unknown"),
            "missing_status" => {
                messages[3]["dial_evidence"]
                    .as_object_mut()
                    .unwrap()
                    .remove("is_error");
            }
            "image" => messages[3]["dial_images"] = json!([{ "path":"image.png" }]),
            "provider_content" => messages[3]["dial_provider_content"] = json!([]),
            "missing_artifact" => {
                messages[3]["dial_evidence"]
                    .as_object_mut()
                    .unwrap()
                    .remove("artifact");
            }
            "unpaired" => messages[2]["tool_calls"] = json!([]),
            _ => {}
        }
        let before = messages.clone();
        let expected: Vec<_> = before.iter().map(crate::llm::messages::canonical).collect();
        for policy in [
            Policy::calm(),
            Policy {
                pressure: true,
                warm_tokens: 0,
                supersede_reads: false,
                drop_useless: false,
                ..Policy::calm()
            },
        ] {
            let mut projected = before.clone();
            project(&mut projected, &mut Supersession::default(), &policy);
            assert_eq!(projected, expected, "must not infer redundancy for {case}");
        }
    }
}

#[test]
fn pressure_keeps_lsp_mutations_unknown_actions_and_uncertain_execution() {
    for action in [
        "rename",
        "rename_file",
        "code_action_apply",
        "execute_command",
        "request",
        "reload",
        "future_action",
        "hover",
    ] {
        let mut messages = exchange("lsp", "lsp", 40_000);
        messages[0]["tool_calls"][0]["function"]["arguments"] =
            json!({"action":action}).to_string().into();
        messages.push(json!({"role":"user","content":"tail"}));
        project(
            &mut messages,
            &mut Supersession::default(),
            &Policy {
                pressure: true,
                protect_tokens: 0,
                ..Policy::calm()
            },
        );
        assert_eq!(
            content(&messages, "lsp").starts_with("[Older"),
            action == "hover",
            "action {action}"
        );
    }
    for (field, value) in [
        ("dial_effects", json!("unknown")),
        ("dial_execution", json!({"effects":"unknown"})),
        ("dial_execution_status", json!("timeout")),
        ("dial_execution", json!({"status":"aborted"})),
    ] {
        let mut messages = exchange("old", "read", 40_000);
        messages[1][field] = value;
        messages[1]["dial_evidence"]["observation"] = json!("same");
        messages.extend(exchange("new", "read", 40_000));
        messages[3]["dial_evidence"]["observation"] = json!("same");
        let old = messages[1].clone();
        project(
            &mut messages,
            &mut Supersession::default(),
            &Policy {
                pressure: true,
                protect_tokens: 0,
                ..Policy::calm()
            },
        );
        assert_eq!(
            messages[1],
            crate::llm::messages::canonical(&old),
            "uncertain execution in {field} stays full"
        );
    }
}

#[test]
fn exact_repeat_filter_respects_cache_policy_and_leaves_small_results_alone() {
    for case in ["small", "cold", "disabled", "pressure"] {
        let size = if case == "small" { 40 } else { 8_000 };
        let mut messages = exchange("old", "grep", size);
        messages.extend(exchange("new", "grep", size));
        messages[1]["dial_evidence"]["is_error"] = json!(false);
        messages[3]["dial_evidence"]["is_error"] = json!(false);
        let mut memo = Supersession::default();
        project(
            &mut messages,
            &mut memo,
            &Policy {
                warm_tokens: if case == "small" { 10_000 } else { 0 },
                pressure: case == "pressure",
                supersede_reads: case != "disabled",
                ..Policy::calm()
            },
        );
        assert_eq!(
            content(&messages, "old").starts_with("[Superseded"),
            case == "pressure",
            "exact duplicate policy for {case}"
        );
        assert!(memo.holds_full("new"));
    }
}

#[test]
fn default_pressure_consolidates_exact_duplicates_below_prune_batch() {
    let mut messages = exchange("old", "grep", 8_000);
    messages.extend(exchange("new", "grep", 8_000));
    messages[1]["dial_evidence"]["is_error"] = json!(false);
    messages[3]["dial_evidence"]["is_error"] = json!(false);
    let original = messages.clone();
    let mut memo = Supersession::default();
    let policy = Policy {
        pressure: true,
        warm_tokens: 0,
        supersede_reads: false,
        drop_useless: false,
        ..Policy::calm()
    };
    project(&mut messages, &mut memo, &policy);
    assert!(content(&messages, "old").len() < 8_000);
    assert_eq!(content(&messages, "new"), content(&original, "new"));
    assert!(!memo.holds_full("old"));
    let mut eased = original.clone();
    project(
        &mut eased,
        &mut memo,
        &Policy {
            pressure: false,
            ..policy
        },
    );
    assert_eq!(messages, eased, "replacement must stay cache-stable");
    assert_eq!(content(&original, "old").len(), 8_000);
}

#[test]
fn supersession_never_expands_small_text_observations() {
    for images in [None, Some(json!([]))] {
        let mut messages = vec![obs("old", "ok", "same"), obs("new", "ok", "same")];
        if let Some(images) = images {
            messages[0]["dial_images"] = images;
        }
        let before = messages.clone();
        let mut memo = Supersession::default();
        project(&mut messages, &mut memo, &Policy::calm());
        let expected: Vec<_> = before.iter().map(crate::llm::messages::canonical).collect();
        assert_eq!(messages, expected);
        assert!(memo.holds_full("old"));
    }
}
