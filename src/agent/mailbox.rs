//! Agent-to-agent mailboxes for one parent agent's team.
//!
//! A parent opens a [`Team`]; its live workers enlist under their job id. A send only pushes onto
//! the recipient's bounded queue and runs that recipient's wake hook; it never waits for the
//! recipient. Recipients drain their queue at their next model step, in send order. Delivery is
//! limited to the sender's own team, and a letter is text only: it carries no authority.
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

/// Reserved recipient naming the team's parent agent.
pub const PARENT: &str = "parent";
/// Reserved recipient naming every other live worker in the team.
pub const ALL: &str = "all";
/// Largest message body accepted.
pub const MAX_TEXT_BYTES: usize = 32 * 1024;
/// Undelivered text one mailbox may hold, on top of its message-count limit.
const MAX_QUEUED_BYTES: usize = 256 * 1024;
/// Recent letters kept per participant, sent or received, for the Workers panel.
const TRAFFIC_KEEP: usize = 6;
/// Characters of a message shown in its traffic preview.
const PREVIEW_CHARS: usize = 80;

/// One delivered message: who sent it, shown to the recipient, and its text.
pub struct Letter {
    pub from: String,
    pub text: String,
}

/// One delivered letter as the Workers panel shows it: sender, recipient, and a short preview.
#[derive(Clone, Debug, PartialEq)]
pub struct Traffic {
    pub from: String,
    pub to: String,
    pub preview: String,
}

pub type Wake = Box<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct Queue {
    letters: VecDeque<Letter>,
    bytes: usize,
}

struct Inbox {
    queue: Mutex<Queue>,
    wake: Option<Wake>,
}

impl Inbox {
    fn new(wake: Option<Wake>) -> Arc<Self> {
        Arc::new(Self {
            queue: Mutex::default(),
            wake,
        })
    }

    /// Queues `letter` unless the count or byte budget is spent. The wake hook runs after the
    /// queue lock is released.
    fn push(&self, letter: Letter, limit: usize) -> Result<(), String> {
        {
            let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
            if queue.letters.len() >= limit {
                return Err(format!("mailbox is full ({limit} undelivered messages)"));
            }
            if queue.bytes + letter.text.len() > MAX_QUEUED_BYTES {
                return Err("mailbox byte budget is full".into());
            }
            queue.bytes += letter.text.len();
            queue.letters.push_back(letter);
        }
        if let Some(wake) = &self.wake {
            wake();
        }
        Ok(())
    }

    fn drain(&self) -> Vec<Letter> {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.bytes = 0;
        Vec::from(std::mem::take(&mut queue.letters))
    }
}

struct Member {
    label: String,
    inbox: Arc<Inbox>,
}

struct Room {
    limit: usize,
    parent: Arc<Inbox>,
    workers: BTreeMap<String, Member>,
    traffic: BTreeMap<String, VecDeque<Traffic>>,
}

/// Every open team, keyed by team id. A team lives while some [`Team`] handle does.
static ROOMS: Mutex<BTreeMap<u64, Room>> = Mutex::new(BTreeMap::new());
static NEXT_ROOM: AtomicU64 = AtomicU64::new(1);

fn rooms() -> std::sync::MutexGuard<'static, BTreeMap<u64, Room>> {
    ROOMS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Handle to one parent's team. Clones share the team; the last drop closes every mailbox.
#[derive(Clone)]
pub struct Team(Arc<Seat>);

struct Seat(u64);

impl Drop for Seat {
    fn drop(&mut self) {
        rooms().remove(&self.0);
    }
}

/// Result of a send: recipients that accepted the letter, and `"recipient: reason"` for those
/// that refused it because their mailbox was full.
#[derive(Debug)]
pub struct Receipt {
    pub delivered: Vec<String>,
    pub refused: Vec<String>,
}

impl Receipt {
    pub fn json(&self) -> Value {
        json!({"delivered": self.delivered, "refused": self.refused})
    }
}

impl Team {
    /// Opens a team whose parent mailbox and each worker mailbox hold at most `limit` letters.
    pub fn open(limit: usize) -> Self {
        let id = NEXT_ROOM.fetch_add(1, Ordering::Relaxed);
        rooms().insert(
            id,
            Room {
                limit,
                parent: Inbox::new(None),
                workers: BTreeMap::new(),
                traffic: BTreeMap::new(),
            },
        );
        Self(Arc::new(Seat(id)))
    }

    fn id(&self) -> u64 {
        self.0.0
    }

    /// Sets the per-mailbox message-count limit (`task.inbox_limit`) for later sends.
    pub fn set_limit(&self, limit: usize) {
        if let Some(room) = rooms().get_mut(&self.id()) {
            room.limit = limit;
        }
    }

    /// Adds a worker mailbox. `wake` runs after each letter is queued for it.
    pub fn enlist(&self, id: &str, label: &str, wake: Wake) -> Result<(), String> {
        let mut rooms = rooms();
        let room = rooms.get_mut(&self.id()).ok_or("message team is closed")?;
        if room.workers.contains_key(id) {
            return Err(format!("{id} is already enlisted"));
        }
        room.workers.insert(
            id.to_owned(),
            Member {
                label: label.to_owned(),
                inbox: Inbox::new(Some(wake)),
            },
        );
        Ok(())
    }

    /// Recipients `who` can address: `parent` (for a worker) and every other live worker, as
    /// `{to, label}` rows. The parent is not listed for itself.
    pub fn roster(&self, who: &str) -> Value {
        let rooms = rooms();
        let Some(room) = rooms.get(&self.id()) else {
            return json!([]);
        };
        let mut rows = Vec::new();
        if who != PARENT {
            rows.push(json!({"to": PARENT}));
        }
        for (id, member) in &room.workers {
            if id != who {
                rows.push(json!({"to": id, "label": member.label}));
            }
        }
        json!(rows)
    }

    /// Removes a finished worker. Letters still queued for it are dropped; later sends to it fail.
    pub fn retire(&self, id: &str) {
        if let Some(room) = rooms().get_mut(&self.id()) {
            room.workers.remove(id);
        }
    }

    /// Takes every queued letter for `who` (`parent` or a worker id), in send order.
    pub fn drain(&self, who: &str) -> Vec<Letter> {
        let inbox = {
            let rooms = rooms();
            let Some(room) = rooms.get(&self.id()) else {
                return Vec::new();
            };
            if who == PARENT {
                room.parent.clone()
            } else {
                match room.workers.get(who) {
                    Some(member) => member.inbox.clone(),
                    None => return Vec::new(),
                }
            }
        };
        inbox.drain()
    }

    /// Sends `text` from `from` (`parent` or a worker id) to `to`. Never blocks on the recipient.
    /// A single recipient that refuses the letter makes the whole send fail; a broadcast reports
    /// each refusal in the receipt.
    pub fn send(&self, from: &str, to: &str, text: &str) -> Result<Receipt, String> {
        if text.trim().is_empty() {
            return Err("message text is empty".into());
        }
        if text.len() > MAX_TEXT_BYTES {
            return Err(format!("message must be at most {MAX_TEXT_BYTES} bytes"));
        }
        let (limit, sender, targets) = {
            let rooms = rooms();
            let room = rooms.get(&self.id()).ok_or("message team is closed")?;
            let sender = if from == PARENT {
                PARENT.to_owned()
            } else {
                let member = room
                    .workers
                    .get(from)
                    .ok_or("sender is no longer a live worker")?;
                display(from, &member.label)
            };
            (room.limit, sender, resolve(room, from, to)?)
        };
        let broadcast = to == ALL;
        let mut receipt = Receipt {
            delivered: Vec::new(),
            refused: Vec::new(),
        };
        for (id, inbox) in targets {
            let letter = Letter {
                from: sender.clone(),
                text: text.to_owned(),
            };
            match inbox.push(letter, limit) {
                Ok(()) => receipt.delivered.push(id),
                Err(why) if broadcast => receipt.refused.push(format!("{id}: {why}")),
                Err(why) => return Err(why),
            }
        }
        if !receipt.delivered.is_empty() {
            self.record(from, &receipt.delivered, text);
        }
        Ok(receipt)
    }

    /// Appends one entry per recipient to the sender's and each recipient's traffic log.
    fn record(&self, from: &str, to: &[String], text: &str) {
        let preview = preview(text);
        let mut rooms = rooms();
        let Some(room) = rooms.get_mut(&self.id()) else {
            return;
        };
        for recipient in to {
            let entry = Traffic {
                from: from.to_owned(),
                to: recipient.clone(),
                preview: preview.clone(),
            };
            keep(
                room.traffic.entry(from.to_owned()).or_default(),
                entry.clone(),
            );
            keep(room.traffic.entry(recipient.clone()).or_default(), entry);
        }
    }

    /// Recent traffic involving `who` (`parent` or a worker id), oldest first.
    pub fn traffic(&self, who: &str) -> Vec<Traffic> {
        rooms()
            .get(&self.id())
            .and_then(|room| room.traffic.get(who))
            .map(|log| log.iter().cloned().collect())
            .unwrap_or_default()
    }
}

fn display(id: &str, label: &str) -> String {
    if label.is_empty() || label == id {
        id.to_owned()
    } else {
        format!("{id} ({label})")
    }
}

/// Appends `entry`, dropping the oldest one once the log holds [`TRAFFIC_KEEP`] entries.
fn keep(log: &mut VecDeque<Traffic>, entry: Traffic) {
    if log.len() == TRAFFIC_KEEP {
        log.pop_front();
    }
    log.push_back(entry);
}

/// First non-blank line of `text`, cut to [`PREVIEW_CHARS`] characters with an ellipsis.
fn preview(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let mut out: String = line.chars().take(PREVIEW_CHARS).collect();
    if line.chars().count() > PREVIEW_CHARS {
        out.push('…');
    }
    out
}

/// Maps a recipient name to its mailboxes. `all` is a broadcast, `parent` is the parent mailbox,
/// and anything else is a worker id, or else a unique worker label.
fn resolve(room: &Room, from: &str, to: &str) -> Result<Vec<(String, Arc<Inbox>)>, String> {
    if to == ALL {
        let targets: Vec<_> = room
            .workers
            .iter()
            .filter(|(id, _)| id.as_str() != from)
            .map(|(id, member)| (id.clone(), member.inbox.clone()))
            .collect();
        if targets.is_empty() {
            return Err("no other live worker to receive a broadcast".into());
        }
        return Ok(targets);
    }
    if to == PARENT {
        if from == PARENT {
            return Err("an agent cannot message itself".into());
        }
        return Ok(vec![(PARENT.to_owned(), room.parent.clone())]);
    }
    let id = match room.workers.get(to) {
        Some(_) => to.to_owned(),
        None => {
            let matches: Vec<&String> = room
                .workers
                .iter()
                .filter(|(_, member)| member.label == to)
                .map(|(id, _)| id)
                .collect();
            match matches.as_slice() {
                [id] => (*id).clone(),
                [] => {
                    return Err(format!(
                        "no live recipient named {to} (unknown or finished); use parent, all, a live task id, or a live task label"
                    ));
                }
                _ => {
                    return Err(format!(
                        "label {to} matches several live workers; address one by task id"
                    ));
                }
            }
        }
    };
    if id == from {
        return Err("an agent cannot message itself".into());
    }
    let inbox = room.workers[&id].inbox.clone();
    Ok(vec![(id, inbox)])
}

#[cfg(test)]
mod tests {
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
}
