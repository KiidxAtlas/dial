use super::*;

const RATE: usize = 16_000;

fn silence(ms: usize) -> Vec<f32> {
    vec![0.0; RATE * ms / 1000]
}

/// Low-level background hiss well under the speech threshold.
fn hiss(ms: usize) -> Vec<f32> {
    (0..RATE * ms / 1000)
        .map(|i| if i % 2 == 0 { 0.002 } else { -0.002 })
        .collect()
}

/// A voiced 220 Hz tone standing in for speech (RMS ≈ 0.21).
fn tone(ms: usize) -> Vec<f32> {
    (0..RATE * ms / 1000)
        .map(|i| 0.3 * (i as f32 * 2.0 * std::f32::consts::PI * 220.0 / RATE as f32).sin())
        .collect()
}

fn segments(events: &[EndpointerEvent]) -> Vec<usize> {
    events
        .iter()
        .map(|e| match e {
            EndpointerEvent::Segment(audio) => audio.len(),
        })
        .collect()
}

fn feed(endpointer: &mut Endpointer, signal: &[f32], chunk: usize) -> Vec<EndpointerEvent> {
    let mut events = Vec::new();
    for part in signal.chunks(chunk) {
        events.extend(endpointer.push(part));
    }
    events
}

#[test]
fn silence_and_hiss_produce_nothing() {
    let mut e = Endpointer::new(EndpointerConfig::default());
    let mut signal = silence(1000);
    signal.extend(hiss(2000));
    let mut events = feed(&mut e, &signal, 512);
    events.extend(e.flush());
    assert!(events.is_empty(), "{events:?}");
}

#[test]
fn one_utterance_between_pauses_is_one_segment_with_pre_roll_and_short_tail() {
    let mut e = Endpointer::new(EndpointerConfig::default());
    let mut signal = hiss(600);
    signal.extend(tone(900));
    signal.extend(hiss(1200));
    let events = feed(&mut e, &signal, 480);
    let lengths = segments(&events);
    assert_eq!(lengths.len(), 1, "{lengths:?}");
    let ms = lengths[0] * 1000 / RATE;
    // 900 ms speech + up to 240 ms pre-roll + ≤120 ms tail.
    assert!((900..=1290).contains(&ms), "segment of {ms} ms");
    assert!(!e.in_speech());
    assert!(e.flush().is_empty());
}

#[test]
fn two_utterances_split_at_the_pause() {
    let mut e = Endpointer::new(EndpointerConfig::default());
    let mut signal = silence(300);
    signal.extend(tone(500));
    signal.extend(silence(800));
    signal.extend(tone(700));
    signal.extend(silence(800));
    assert_eq!(segments(&feed(&mut e, &signal, 1000)).len(), 2);
}

#[test]
fn short_pause_does_not_split() {
    let mut e = Endpointer::new(EndpointerConfig::default());
    let mut signal = tone(400);
    signal.extend(silence(300));
    signal.extend(tone(400));
    signal.extend(silence(800));
    assert_eq!(segments(&feed(&mut e, &signal, 320)).len(), 1);
}

#[test]
fn clicks_shorter_than_min_speech_are_dropped() {
    let mut e = Endpointer::new(EndpointerConfig::default());
    let mut signal = silence(500);
    signal.extend(tone(60));
    signal.extend(silence(1000));
    assert!(segments(&feed(&mut e, &signal, 160)).is_empty());
}

#[test]
fn long_speech_commits_at_the_segment_cap() {
    let mut e = Endpointer::new(EndpointerConfig::default());
    let events = feed(&mut e, &tone(13_000), 1600);
    let lengths = segments(&events);
    assert_eq!(lengths.len(), 1);
    assert!(lengths[0] * 1000 / RATE >= 11_900);
    let tail = segments(&e.flush());
    assert_eq!(tail.len(), 1, "remaining speech is committed by flush");
}

#[test]
fn flush_commits_pending_speech_and_resets() {
    let mut e = Endpointer::new(EndpointerConfig::default());
    let mut signal = silence(200);
    signal.extend(tone(500));
    assert!(segments(&feed(&mut e, &signal, 333)).is_empty());
    assert_eq!(segments(&e.flush()).len(), 1);
    assert!(!e.in_speech());
    assert!(e.flush().is_empty());
}

#[test]
fn chunking_does_not_change_results() {
    let mut signal = hiss(400);
    signal.extend(tone(700));
    signal.extend(hiss(900));
    signal.extend(tone(300));
    signal.extend(hiss(900));
    let mut a = Endpointer::new(EndpointerConfig::default());
    let mut b = Endpointer::new(EndpointerConfig::default());
    let whole = segments(&feed(&mut a, &signal, signal.len()));
    let odd = segments(&feed(&mut b, &signal, 77));
    assert_eq!(whole, odd);
    assert_eq!(whole.len(), 2);
}

#[test]
fn noise_floor_adapts_to_a_louder_room() {
    // Constant background at RMS 0.012 sits above the absolute floor; after adaptation it
    // must not read as speech, while a much louder tone still does.
    let room: Vec<f32> = (0..RATE * 3)
        .map(|i| if i % 2 == 0 { 0.012 } else { -0.012 })
        .collect();
    let mut e = Endpointer::new(EndpointerConfig::default());
    let early = segments(&feed(&mut e, &room, 480));
    // The very first frames may trip before the floor adapts; afterwards the room is silence.
    let mut later = room.clone();
    later.extend(tone(600));
    later.extend(room.iter().take(RATE));
    let after = segments(&feed(&mut e, &later, 480));
    assert!(early.len() <= 1, "{early:?}");
    assert_eq!(after.len(), 1, "{after:?}");
}
