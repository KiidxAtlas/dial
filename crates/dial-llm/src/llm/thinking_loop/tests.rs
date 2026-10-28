use super::*;

fn feed(text: &str) -> Option<String> {
    let mut guard = ThinkingLoopGuard::new(true);
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(17)
        .find_map(|chunk| guard.push(&chunk.iter().collect::<String>()))
        .or_else(|| guard.flush())
}

/// Deterministic pseudo-random lowercase words, so no cycle or shared vocabulary exists.
fn random_words(seed: u32, count: usize) -> Vec<String> {
    let mut state = seed;
    (0..count)
        .map(|_| {
            (0..7)
                .map(|_| {
                    state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                    char::from(b'a' + ((state >> 16) % 26) as u8)
                })
                .collect()
        })
        .collect()
}

#[test]
fn verbatim_back_to_back_repetition_trips() {
    assert!(feed(&"🌊 ".repeat(120)).is_some());
    assert!(feed(&"I will now check the file again. ".repeat(8)).is_some());
}

#[test]
fn formatting_runs_and_brief_repeats_are_not_loops() {
    for text in [
        "-".repeat(500),
        " ".repeat(500),
        "| --- ".repeat(100),
        "00 ".repeat(200),
        "🌊 ".repeat(26),
        format!("intro\n{}\n", "=".repeat(400)),
    ] {
        assert_eq!(feed(&text), None, "{text:?}");
    }
}

#[test]
fn long_cycles_need_three_repeats_and_1024_characters() {
    // A pseudo-random unit, so no shorter cycle exists inside it.
    let mut state: u32 = 12345;
    let unit: String = (0..300)
        .map(|_| {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            char::from(b'a' + ((state >> 16) % 26) as u8)
        })
        .collect::<String>()
        + ".";
    assert_eq!(
        feed(&unit.repeat(3)),
        None,
        "903 repeated characters is below the long-cycle floor"
    );
    assert!(feed(&unit.repeat(4)).is_some());
}

#[test]
fn bounded_tail_handles_large_stream() {
    let mut guard = ThinkingLoopGuard::new(true);
    assert!(guard.push(&"x".repeat(20_000)).is_some());
    assert!(guard.tail.len() <= 2 * EXACT_TAIL_WINDOW);
}

#[test]
fn distinct_paragraphs_do_not_trip() {
    let mut guard = ThinkingLoopGuard::new(true);
    for (index, words) in random_words(7, 40).chunks(6).enumerate() {
        let paragraph = format!(
            "Step {index}: {}. Next I weigh the {} option carefully.\n\n",
            words.join(" "),
            words[0]
        );
        assert_eq!(guard.push(&paragraph), None, "paragraph {index}");
    }
    assert_eq!(guard.flush(), None);
}

#[test]
fn near_duplicate_paragraphs_trip_on_cluster() {
    let mut guard = ThinkingLoopGuard::new(true);
    let mut hit = None;
    for index in 0..10 {
        let paragraph = format!(
            "We should inspect the parser module next and then compare the output with fixture number {index} before deciding anything else.\n\n"
        );
        hit = hit.or_else(|| guard.push(&paragraph));
    }
    let reason = hit.expect("near-duplicate paragraphs must trip");
    assert!(reason.contains("near-identical segments"), "{reason}");
}

#[test]
fn recycled_vocabulary_without_new_anchors_trips_progress_stall() {
    let vocabulary = random_words(99, 12);
    let mut guard = ThinkingLoopGuard::new(true);
    let mut hit = None;
    for round in 0..12 {
        let mut words = vocabulary.clone();
        words.rotate_left((round * 5) % 12);
        words.reverse();
        hit = hit.or_else(|| guard.push(&format!("{}.\n\n", words.join(" "))));
    }
    assert!(
        hit.is_some(),
        "recycled wording with no new anchors must trip"
    );
}

#[test]
fn flush_catches_a_trailing_duplicate_paragraph() {
    let mut guard = ThinkingLoopGuard::new(true);
    let paragraph = |index: usize| {
        format!(
            "We should inspect the parser module next and then compare the output with fixture number {index} before deciding anything else."
        )
    };
    for index in 0..7 {
        assert_eq!(guard.push(&format!("{}\n\n", paragraph(index))), None);
    }
    assert!(guard.push(&paragraph(7)).is_none());
    assert!(guard.flush().is_some());
}

#[test]
fn code_titles_and_headings_do_not_count_as_prose() {
    let mut guard = ThinkingLoopGuard::new(true);
    for index in 0..12 {
        assert_eq!(
            guard.push(&format!(
                "## Plan {index}\n\n**Maintaining Momentum {index}**\n\n    let x{index} = y;\n\n"
            )),
            None
        );
    }
}

#[test]
fn gemini_header_run_trips_at_thirty_six() {
    let mut thirty_five = GeminiHeaderRunDetector::default();
    let text: String = (0..35).map(|i| format!("## Plan step {i}\n")).collect();
    assert_eq!(thirty_five.push(&text), None);

    let mut thirty_six = GeminiHeaderRunDetector::default();
    let text: String = (0..36).map(|i| format!("## Plan step {i}\n")).collect();
    assert_eq!(
        thirty_six.push(&text),
        Some(GEMINI_HEADER_RUNAWAY_THRESHOLD)
    );
    assert_eq!(thirty_six.push("## again\n"), None, "latches after firing");
}

#[test]
fn policy_scopes_semantic_and_header_detectors_by_model_family() {
    let gemini = policy_with("google/gemini-2.5-pro", false).unwrap();
    assert!(gemini.semantic && gemini.header);
    let deepseek = policy_with("deepseek-reasoner", false).unwrap();
    assert!(deepseek.semantic && !deepseek.header);
    let other = policy_with("anthropic/claude-sonnet", false).unwrap();
    assert!(!other.semantic && !other.header);
    assert_eq!(
        policy_with("gemini-2.5-pro", true),
        None,
        "kill switch disables every detector"
    );
}

#[test]
fn loop_detectors_trust_only_a_leading_basename_family() {
    let contained = policy_with("vertex-gemini-2", false).unwrap();
    assert!(
        !contained.semantic && !contained.header,
        "name containment must not grant loop detectors"
    );
    let custom = policy_with("claude-gemini-proxy", false).unwrap();
    assert!(
        !custom.semantic && !custom.header,
        "a custom name is neither rejected nor given gemini detectors"
    );
}
