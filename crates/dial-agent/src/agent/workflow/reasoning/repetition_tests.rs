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
    assert!(GateHit(gate.hit.unwrap()).is_repetition());
}
