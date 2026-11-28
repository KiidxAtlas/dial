use super::*;

#[test]
fn exact_cycles_are_detected_across_chunk_boundaries() {
    let text = "I should inspect the same evidence before proceeding. ".repeat(40);
    for size in [1, 7, 128, text.len()] {
        let mut detector = Repetition::default();
        assert!(
            text.as_bytes()
                .chunks(size)
                .any(|chunk| { detector.push(std::str::from_utf8(chunk).unwrap()) })
        );
    }
    let mut detector = Repetition::default();
    assert!(detector.push(&"重复的推理需要停止而不是继续重复。".repeat(80)));
}

#[test]
fn long_unique_text_short_repeats_and_whitespace_do_not_trip() {
    let mut detector = Repetition::default();
    for i in 0..1000 {
        assert!(!detector.push(&format!("Record {i:04}: this line has unique evidence.\n")));
    }
    assert!(detector.tail.len() < 8320);
    assert!(!Repetition::default().push(&"a brief repeated phrase. ".repeat(3)));
    assert!(!Repetition::default().push(&" ".repeat(10000)));
}

#[test]
fn tool_payloads_are_not_treated_as_repetitive_answer_text() {
    let mut gate = Gate::new();
    gate.tools_started = true;
    assert!(!gate.repeated(&"large repeated file content. ".repeat(100), false));
    assert!(gate.repeated(&"I keep thinking the same thing. ".repeat(100), true));
    assert!(GateHit::new(gate.hit.unwrap(), None).is_repetition());
}

#[test]
fn long_reasoning_exact_cycle_catches_on_third_repeat_while_text_requires_four() {
    let para = "Hmm, so how did getrandom get bumped? Let me check the actual behavior by looking at the output more carefully. Actually, maybe the regex matched the version line of getrandom because the alternation was not parenthesized properly.\n\n";
    assert!(para.len() >= 160);
    // 3.5 copies crosses the 128-byte scan cadence right after repeat #3 while staying below 4.
    let three_plus = format!("{}{}", para.repeat(3), &para[..128]);
    let mut thinking_gate = Gate::new();
    assert!(thinking_gate.repeated(&three_plus, true));
    assert!(GateHit::new(thinking_gate.hit.unwrap(), None).is_repetition());

    let mut text_gate = Gate::new();
    assert!(
        !text_gate.repeated(&three_plus, false),
        "answer text keeps the stricter 4-repeat threshold"
    );
}

#[test]
fn reasoning_loop_detector_catches_paraphrase_spiral_and_interleaved_paragraph_replay() {
    // Shape 1: L631-style paraphrase / opener-drift spiral (varying openers, repeating clauses).
    let openers = [
        "Oh wait — I see.",
        "Wait — maybe the issue is.",
        "Oh — I see now.",
        "Hmm, maybe the issue is.",
        "Oh! I see what happened.",
        "Actually, let me check again.",
        "Wait, let me look closer.",
        "Hmm, so how did that happen?",
    ];
    let core = " The regex name = dial-agent or name = dial newline version = anything splits into two alternatives and somehow matched getrandom hex and mach2 version lines in the lockfile output. Let me check the actual behavior by looking at the grep output more carefully to see why getrandom got version 0.4.3.";
    let mut spiral = String::new();
    for opener in openers.iter().cycle().take(12) {
        spiral.push_str(opener);
        spiral.push_str(core);
        spiral.push_str("\n\n");
    }
    for chunk_size in [13, 128, spiral.len()] {
        let mut gate = Gate::new();
        let mut tripped = false;
        let mut pos = 0;
        while pos < spiral.len() {
            let mut end = (pos + chunk_size).min(spiral.len());
            while end < spiral.len() && !spiral.is_char_boundary(end) {
                end += 1;
            }
            if gate.repeated(&spiral[pos..end], true) {
                tripped = true;
                break;
            }
            pos = end;
        }
        assert!(tripped, "spiral should trip for chunk_size={chunk_size}");
        assert!(GateHit::new(gate.hit.unwrap(), None).is_repetition());
    }

    // Shape 2: STEP#44-style interleaved verbatim paragraph replay separated by a reconsider paragraph.
    let p_clean = "Now let me write the Makefile. Let me also think about the clean target. The user wants make clean to delete all the rust build output across dial-engine and dial-desktop safely while skipping files currently in use by another process.";
    let p_repo = "I will run the per-repo clean targets directly. The engine clean uses scripts/clean.py which removes target safely and skips dirs in use. The desktop clean removes src-tauri/target and dist without touching uncommitted source changes.";
    let p_reconsider = "Actually, let me reconsider the Makefile syntax once more before editing. The user wrote make version desktop or engine with a version argument, so let me verify how Make parses arguments cleanly.";
    let interleaved = format!(
        "{p_clean}\n\n{p_repo}\n\n{p_reconsider}\n\n{p_clean}\n\n{p_repo}\n\nNow let me write the scripts and verify the clean target works safely across both repos without touching uncommitted work.\n\n"
    );
    let mut gate = Gate::new();
    assert!(gate.repeated(&interleaved, true));
    assert!(GateHit::new(gate.hit.unwrap(), None).is_repetition());

    // Healthy long structured reasoning with distinct paragraphs must NOT trip.
    let mut healthy = String::new();
    for i in 0..25 {
        healthy.push_str(&format!(
            "Step {i}: inspect module component_{i} in crate dial_part_{i}, verify symbol handle_{i} signature, check error path branch_{i}, and confirm unit test coverage_{i} passes without regressions.\n\n"
        ));
    }
    let mut healthy_gate = Gate::new();
    assert!(!healthy_gate.repeated(&healthy, true));
}
