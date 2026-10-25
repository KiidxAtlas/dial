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

/// A recipient that did not accept a letter, and why.
#[derive(Debug, PartialEq)]
pub struct Refusal {
    pub to: String,
    pub reason: String,
}

/// Per-recipient outcome of a send. A letter a mailbox accepted is `queued`: it is delivered when
/// the recipient drains its mailbox at its next model step, which the sender cannot know
/// synchronously, so `delivered` is never claimed. A recipient that could not take the letter
/// (full mailbox) is listed with its reason. Finished or unknown recipients fail the whole send.
#[derive(Debug)]
pub struct Receipt {
    pub queued: Vec<String>,
    pub refused: Vec<Refusal>,
}

impl Receipt {
    /// One `{to, status}` row per recipient, `status` being `queued` or `refused` (with `reason`).
    pub fn json(&self) -> Value {
        let rows: Vec<Value> = self
            .queued
            .iter()
            .map(|to| json!({"to": to, "status": "queued"}))
            .chain(
                self.refused
                    .iter()
                    .map(|r| json!({"to": r.to, "status": "refused", "reason": r.reason})),
            )
            .collect();
        json!({"receipts": rows})
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
            queued: Vec::new(),
            refused: Vec::new(),
        };
        for (id, inbox) in targets {
            let letter = Letter {
                from: sender.clone(),
                text: text.to_owned(),
            };
            match inbox.push(letter, limit) {
                Ok(()) => receipt.queued.push(id),
                Err(reason) if broadcast => receipt.refused.push(Refusal { to: id, reason }),
                Err(why) => return Err(why),
            }
        }
        if !receipt.queued.is_empty() {
            self.record(from, &receipt.queued, text);
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
mod tests;
