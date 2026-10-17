use super::*;
use std::sync::atomic::AtomicUsize;

fn counter() -> (Arc<AtomicUsize>, Wake) {
    let count = Arc::new(AtomicUsize::new(0));
    let hook = count.clone();
    (
        count,
        Box::new(move || {
            hook.fetch_add(1, Ordering::SeqCst);
        }),
    )
}

fn quiet() -> Wake {
    Box::new(|| {})
}

#[test]
fn send_never_waits_for_the_recipient_and_wakes_it_per_letter() {
    let team = Team::open(8);
    let (woken, wake) = counter();
    team.enlist("task-1", "scout", wake).unwrap();
    // The recipient never drains; every send still returns at once.
    for n in 0..3 {
        let receipt = team.send(PARENT, "task-1", &format!("note {n}")).unwrap();
        assert_eq!(receipt.delivered, ["task-1"]);
    }
    assert_eq!(woken.load(Ordering::SeqCst), 3);
}

#[test]
fn delivery_keeps_send_order_and_is_taken_once() {
    let team = Team::open(8);
    team.enlist("task-1", "scout", quiet()).unwrap();
    team.send(PARENT, "task-1", "first").unwrap();
    team.send(PARENT, "task-1", "second").unwrap();
    team.send(PARENT, "task-1", "third").unwrap();
    let texts: Vec<String> = team.drain("task-1").into_iter().map(|l| l.text).collect();
    assert_eq!(texts, ["first", "second", "third"]);
    assert!(
        team.drain("task-1").is_empty(),
        "a letter is delivered once"
    );
}

#[test]
fn letters_sent_after_a_step_arrive_at_the_next_step() {
    let team = Team::open(8);
    team.enlist("task-1", "scout", quiet()).unwrap();
    team.send(PARENT, "task-1", "before").unwrap();
    assert_eq!(team.drain("task-1").len(), 1);
    team.send(PARENT, "task-1", "after").unwrap();
    let next = team.drain("task-1");
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].text, "after");
}

#[test]
fn a_full_mailbox_refuses_explicitly_and_keeps_its_queue() {
    let team = Team::open(2);
    team.enlist("task-1", "scout", quiet()).unwrap();
    team.send(PARENT, "task-1", "one").unwrap();
    team.send(PARENT, "task-1", "two").unwrap();
    let why = team.send(PARENT, "task-1", "three").err().unwrap();
    assert!(why.contains("mailbox is full"), "{why}");
    let kept: Vec<String> = team.drain("task-1").into_iter().map(|l| l.text).collect();
    assert_eq!(kept, ["one", "two"]);
    team.send(PARENT, "task-1", "four").unwrap();
}

#[test]
fn the_byte_budget_refuses_before_the_count_limit() {
    let team = Team::open(100);
    team.enlist("task-1", "scout", quiet()).unwrap();
    let body = "x".repeat(MAX_TEXT_BYTES);
    for _ in 0..8 {
        team.send(PARENT, "task-1", &body).unwrap();
    }
    let why = team.send(PARENT, "task-1", &body).err().unwrap();
    assert!(why.contains("byte budget"), "{why}");
}

#[test]
fn oversized_and_empty_text_are_refused() {
    let team = Team::open(8);
    team.enlist("task-1", "scout", quiet()).unwrap();
    assert!(team.send(PARENT, "task-1", "   ").is_err());
    let big = "x".repeat(MAX_TEXT_BYTES + 1);
    assert!(
        team.send(PARENT, "task-1", &big)
            .unwrap_err()
            .contains("at most")
    );
}

#[test]
fn unknown_and_finished_recipients_fail_clearly() {
    let team = Team::open(8);
    team.enlist("task-1", "scout", quiet()).unwrap();
    let unknown = team.send(PARENT, "task-9", "hi").unwrap_err();
    assert!(unknown.contains("unknown or finished"), "{unknown}");
    team.retire("task-1");
    assert!(team.send(PARENT, "task-1", "late").is_err());
}

#[test]
fn a_worker_reaches_the_parent_and_the_parent_reaches_a_worker() {
    let team = Team::open(8);
    team.enlist("task-1", "scout", quiet()).unwrap();
    let receipt = team.send("task-1", PARENT, "found it").unwrap();
    assert_eq!(receipt.delivered, [PARENT]);
    let letters = team.drain(PARENT);
    assert_eq!(letters[0].from, "task-1 (scout)");
    assert_eq!(letters[0].text, "found it");
}

#[test]
fn a_worker_broadcast_reaches_siblings_but_not_itself_or_the_parent() {
    let team = Team::open(8);
    team.enlist("task-1", "a", quiet()).unwrap();
    team.enlist("task-2", "b", quiet()).unwrap();
    team.enlist("task-3", "c", quiet()).unwrap();
    let receipt = team.send("task-1", ALL, "status").unwrap();
    assert_eq!(receipt.delivered, ["task-2", "task-3"]);
    assert!(team.drain("task-1").is_empty());
    assert!(team.drain(PARENT).is_empty());
    assert_eq!(team.drain("task-2").len(), 1);
}

#[test]
fn a_parent_broadcast_reaches_every_worker() {
    let team = Team::open(8);
    team.enlist("task-1", "a", quiet()).unwrap();
    team.enlist("task-2", "b", quiet()).unwrap();
    let receipt = team.send(PARENT, ALL, "regroup").unwrap();
    assert_eq!(receipt.delivered, ["task-1", "task-2"]);
}

#[test]
fn a_broadcast_with_no_recipients_is_an_error() {
    let team = Team::open(8);
    team.enlist("task-1", "a", quiet()).unwrap();
    assert!(team.send("task-1", ALL, "anyone?").is_err());
}

#[test]
fn traffic_keeps_only_the_newest_entries_per_participant() {
    let team = Team::open(64);
    team.enlist("task-1", "scout", quiet()).unwrap();
    for n in 0..TRAFFIC_KEEP + 3 {
        team.send("task-1", PARENT, &format!("report {n}")).unwrap();
    }
    let sent = team.traffic("task-1");
    assert_eq!(sent.len(), TRAFFIC_KEEP);
    assert_eq!(sent[0].preview, "report 3");
    assert_eq!(
        sent.last().unwrap().preview,
        format!("report {}", TRAFFIC_KEEP + 2)
    );
    assert_eq!(team.traffic(PARENT).len(), TRAFFIC_KEEP);
}

#[test]
fn traffic_records_only_delivered_letters_and_both_endpoints() {
    let team = Team::open(1);
    team.enlist("task-1", "a", quiet()).unwrap();
    team.enlist("task-2", "b", quiet()).unwrap();
    team.send(PARENT, "task-2", "queued").unwrap();
    assert!(team.send(PARENT, "task-2", "refused").is_err());
    assert!(team.send(PARENT, "task-9", "unknown").is_err());
    let log = team.traffic("task-2");
    assert_eq!(
        log,
        [Traffic {
            from: PARENT.into(),
            to: "task-2".into(),
            preview: "queued".into(),
        }]
    );
    assert!(team.traffic("task-1").is_empty());
}

#[test]
fn preview_is_one_bounded_line() {
    assert_eq!(preview("\n  first line \nsecond"), "first line");
    let long = "é".repeat(PREVIEW_CHARS + 5);
    let cut = preview(&long);
    assert_eq!(cut.chars().count(), PREVIEW_CHARS + 1);
    assert!(cut.ends_with('…'));
}

#[test]
fn a_broadcast_reports_full_mailboxes_without_failing_the_rest() {
    let team = Team::open(1);
    team.enlist("task-1", "a", quiet()).unwrap();
    team.enlist("task-2", "b", quiet()).unwrap();
    team.enlist("task-3", "c", quiet()).unwrap();
    team.send(PARENT, "task-2", "already queued").unwrap();
    let receipt = team.send(PARENT, ALL, "news").unwrap();
    assert_eq!(receipt.delivered, ["task-1", "task-3"]);
    assert_eq!(receipt.refused.len(), 1);
    assert!(receipt.refused[0].starts_with("task-2: mailbox is full"));
}

#[test]
fn labels_resolve_only_when_unambiguous() {
    let team = Team::open(8);
    team.enlist("task-1", "scout", quiet()).unwrap();
    team.enlist("task-2", "scout", quiet()).unwrap();
    team.enlist("task-3", "reviewer", quiet()).unwrap();
    let ambiguous = team.send(PARENT, "scout", "hi").unwrap_err();
    assert!(ambiguous.contains("several live workers"), "{ambiguous}");
    assert_eq!(
        team.send(PARENT, "reviewer", "hi").unwrap().delivered,
        ["task-3"]
    );
}

#[test]
fn an_agent_cannot_message_itself() {
    let team = Team::open(8);
    team.enlist("task-1", "scout", quiet()).unwrap();
    assert!(team.send("task-1", "task-1", "me").is_err());
    assert!(team.send("task-1", "scout", "me").is_err());
    assert!(team.send(PARENT, PARENT, "me").is_err());
}

#[test]
fn teams_are_isolated_from_each_other() {
    let first = Team::open(8);
    let second = Team::open(8);
    first.enlist("task-1", "scout", quiet()).unwrap();
    assert!(second.send(PARENT, "task-1", "wrong team").is_err());
    assert!(second.drain("task-1").is_empty());
}
