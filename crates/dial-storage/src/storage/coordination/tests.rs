use super::*;
use std::{
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Fixture {
    _root: tempfile::TempDir,
    layout: Layout,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir(&project).unwrap();
        let layout = Layout::with_user_dir(&project, root.path().join("private")).unwrap();
        Self {
            _root: root,
            layout,
        }
    }
    fn dir(&self) -> PathBuf {
        self.layout.state_dir.join("coordination")
    }
}

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn child(fixture: &Fixture, mode: &str, name: &str, to: &str) -> Process {
    Process(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "storage::coordination::tests::subprocess_helper",
                "--nocapture",
            ])
            .env("DIAL_COORDINATION_TEST_MODE", mode)
            .env(
                "DIAL_COORDINATION_TEST_PROJECT",
                &fixture.layout.project_root,
            )
            .env("DIAL_COORDINATION_TEST_HOME", &fixture.layout.user_dir)
            .env("DIAL_COORDINATION_TEST_NAME", name)
            .env("DIAL_COORDINATION_TEST_TO", to)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    )
}
fn wait_for(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "subprocess coordination timed out"
        );
        thread::sleep(Duration::from_millis(10));
    }
}
fn wait_success(process: &mut Process) {
    let mut status = None;
    wait_for(|| {
        status = process.0.try_wait().unwrap();
        status.is_some()
    });
    assert!(status.unwrap().success());
}

/// An ordinary test invocation is a no-op; the parent test starts this exact test in another OS
/// process with private fixture paths. No ambient environment is changed by the parent tests.
#[test]
fn subprocess_helper() {
    let Ok(mode) = std::env::var("DIAL_COORDINATION_TEST_MODE") else {
        return;
    };
    let project = PathBuf::from(std::env::var_os("DIAL_COORDINATION_TEST_PROJECT").unwrap());
    let home = PathBuf::from(std::env::var_os("DIAL_COORDINATION_TEST_HOME").unwrap());
    let name = std::env::var("DIAL_COORDINATION_TEST_NAME").unwrap();
    let layout = Layout::with_user_dir(&project, home).unwrap();
    let participant = Participant::join(&layout, "shared-journal").unwrap();
    if mode == "send" {
        let to = std::env::var("DIAL_COORDINATION_TEST_TO").unwrap();
        participant.send(&to, &name).unwrap();
        return;
    }
    participant
        .publish("subprocess", "editing", &["src/main.rs".into()])
        .unwrap();
    let _registry = if mode == "lock" {
        Some(transaction(&layout.state_dir.join("coordination")).unwrap())
    } else {
        None
    };
    atomic::write_private(
        &project.join(format!("{name}.ready")),
        participant.id().as_bytes(),
    )
    .unwrap();
    if mode == "mail" {
        let mut letters = Vec::new();
        wait_for(|| {
            letters.extend(participant.drain().unwrap());
            letters.len() == 8
        });
        atomic::write_private(
            &project.join(format!("{name}.letters")),
            &serde_json::to_vec(&letters).unwrap(),
        )
        .unwrap();
    }
    wait_for(|| project.join(format!("{name}.stop")).exists());
}

#[test]
fn unique_instances_canonical_scopes_and_layout_isolation() {
    let fixture = Fixture::new();
    let first = Participant::join(&fixture.layout, "same-session").unwrap();
    let second = Participant::join(&fixture.layout, "same-session").unwrap();
    assert_ne!(first.id(), second.id());
    second
        .publish(
            "fixing parser",
            "editing",
            &["src/parser.rs".into(), "src/parser.rs".into()],
        )
        .unwrap();
    let peers = first.peers().unwrap();
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].scopes, ["src/parser.rs"]);
    let other = Fixture::new();
    assert!(
        Participant::join(&other.layout, "other")
            .unwrap()
            .peers()
            .unwrap()
            .is_empty()
    );
    assert!(first.send("../escape", "hi").is_err());
    assert!(first.send(first.id(), "hi").is_err());
    for bad in [
        "../src",
        "/src",
        "src/../secret",
        "C:\\src",
        "",
        "./src",
        "src//parser.rs",
        "src/",
    ] {
        assert!(
            first.publish("", "", &[bad.into()]).is_err(),
            "accepted {bad:?}"
        );
    }
    assert!(first.publish(&"x".repeat(1025), "", &[]).is_err());
    assert!(first.publish("", "", &vec!["a".into(); 33]).is_err());
    drop(second);
    assert!(first.peers().unwrap().is_empty());
}

#[test]
fn cross_process_presence_delivery_and_clean_exit() {
    let fixture = Fixture::new();
    let observer = Participant::join(&fixture.layout, "observer").unwrap();
    let mut recipient = child(&fixture, "mail", "recipient", "");
    let ready = fixture.layout.project_root.join("recipient.ready");
    wait_for(|| ready.exists());
    let id = fs::read_to_string(ready).unwrap();
    let peers = observer.peers().unwrap();
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].id, id);
    assert_eq!(peers[0].summary, "subprocess");
    let mut senders: Vec<_> = (0..8)
        .map(|i| child(&fixture, "send", &format!("message-{i}"), &id))
        .collect();
    for sender in &mut senders {
        wait_success(sender);
    }
    let letters_path = fixture.layout.project_root.join("recipient.letters");
    wait_for(|| letters_path.exists());
    let letters: Vec<Letter> = serde_json::from_slice(&fs::read(letters_path).unwrap()).unwrap();
    let mut messages: Vec<_> = letters.iter().map(|l| l.text.clone()).collect();
    messages.sort();
    assert_eq!(
        messages,
        (0..8).map(|i| format!("message-{i}")).collect::<Vec<_>>()
    );
    assert!(letters.iter().all(|l| valid_id(&l.from)));
    fs::write(fixture.layout.project_root.join("recipient.stop"), "").unwrap();
    wait_success(&mut recipient);
    assert!(observer.peers().unwrap().is_empty());
    assert!(observer.send(&id, "too late").is_err());
}

#[test]
fn crash_releases_liveness_and_reaps_stale_files() {
    let fixture = Fixture::new();
    let observer = Participant::join(&fixture.layout, "observer").unwrap();
    let mut peer = child(&fixture, "hold", "crash", "");
    let ready = fixture.layout.project_root.join("crash.ready");
    wait_for(|| ready.exists());
    let id = fs::read_to_string(ready).unwrap();
    assert_eq!(observer.peers().unwrap().len(), 1);
    peer.0.kill().unwrap();
    peer.0.wait().unwrap();
    assert!(fixture.dir().join(format!("{id}.json")).exists());
    assert!(observer.send(&id, "not delivered").is_err());
    assert!(observer.peers().unwrap().is_empty());
    assert!(!fixture.dir().join(format!("{id}.json")).exists());
    assert!(!fixture.dir().join(format!("{id}.live")).exists());
}

#[test]
fn full_inbox_rejects_without_eviction_and_drain_is_exact() {
    let fixture = Fixture::new();
    let sender = Participant::join(&fixture.layout, "sender").unwrap();
    let recipient = Participant::join(&fixture.layout, "recipient").unwrap();
    for i in 0..MAX_MESSAGES {
        sender.send(recipient.id(), &format!("letter-{i}")).unwrap();
    }
    assert!(sender.send(recipient.id(), "overflow").is_err());
    for i in 0..MAX_MESSAGES / 2 {
        let batch = recipient.drain().unwrap();
        assert_eq!(
            batch.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            [format!("letter-{}", i * 2), format!("letter-{}", i * 2 + 1)]
        );
    }
    assert!(recipient.drain().unwrap().is_empty());
    let large = "x".repeat(MAX_TEXT);
    let mut accepted = 0;
    while sender.send(recipient.id(), &large).is_ok() {
        accepted += 1;
    }
    assert_eq!(accepted, 7); // serialized envelope counts toward the 64 KiB limit
    let mut drained = 0;
    loop {
        let batch = recipient.drain().unwrap();
        assert!(batch.len() <= 2);
        assert!(batch.iter().map(|letter| letter.text.len()).sum::<usize>() <= 16 * 1024);
        if batch.is_empty() {
            break;
        }
        drained += batch.len();
    }
    assert_eq!(drained, accepted);
    assert!(
        sender
            .send(recipient.id(), &"x".repeat(MAX_TEXT + 1))
            .is_err()
    );
    assert!(sender.send(recipient.id(), "\0").is_err());
}

#[test]
fn malformed_records_are_hidden_and_never_overwritten_by_delivery() {
    let fixture = Fixture::new();
    let sender = Participant::join(&fixture.layout, "sender").unwrap();
    let recipient = Participant::join(&fixture.layout, "recipient").unwrap();
    let path = fixture.dir().join(format!("{}.json", recipient.id()));
    fs::write(&path, b"not json").unwrap();
    assert!(sender.peers().unwrap().is_empty());
    assert!(sender.send(recipient.id(), "no").is_err());
    assert_eq!(fs::read(&path).unwrap(), b"not json");
    fs::write(&path, vec![b' '; MAX_RECORD + 1]).unwrap();
    assert!(sender.send(recipient.id(), "no").is_err());
    assert!(recipient.drain().is_err());
}

#[cfg(unix)]
#[test]
fn unsafe_files_and_directory_are_refused_without_following_links() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let outside = fixture.layout.project_root.join("outside");
    fs::write(&outside, "untouched").unwrap();
    atomic::private_dir(&fixture.layout.state_dir).unwrap();
    symlink(&fixture.layout.project_root, fixture.dir()).unwrap();
    assert!(Participant::join(&fixture.layout, "session").is_err());
    fs::remove_file(fixture.dir()).unwrap();
    let sender = Participant::join(&fixture.layout, "sender").unwrap();
    let recipient = Participant::join(&fixture.layout, "recipient").unwrap();
    let record = fixture.dir().join(format!("{}.json", recipient.id()));
    fs::remove_file(&record).unwrap();
    symlink(&outside, &record).unwrap();
    assert!(sender.send(recipient.id(), "no").is_err());
    assert_eq!(fs::read_to_string(outside).unwrap(), "untouched");
}

#[test]
fn registry_capacity_counts_even_malformed_live_instances() {
    let fixture = Fixture::new();
    let mut participants = Vec::new();
    for _ in 0..MAX_PEERS {
        participants.push(Participant::join(&fixture.layout, "shared-session").unwrap());
    }
    assert!(Participant::join(&fixture.layout, "overflow").is_err());
    let path = fixture.dir().join(format!("{}.json", participants[0].id()));
    fs::write(path, b"bad record").unwrap();
    assert!(Participant::join(&fixture.layout, "still full").is_err());
    participants.pop();
    assert!(Participant::join(&fixture.layout, "now available").is_ok());
}

#[test]
fn stuck_process_registry_lock_has_bounded_wait_and_crash_recovery() {
    let fixture = Fixture::new();
    let observer = Participant::join(&fixture.layout, "observer").unwrap();
    let mut peer = child(&fixture, "lock", "locked", "");
    wait_for(|| fixture.layout.project_root.join("locked.ready").exists());
    let start = Instant::now();
    assert!(observer.peers().unwrap_err().to_string().contains("busy"));
    assert!(start.elapsed() < Duration::from_secs(5));
    peer.0.kill().unwrap();
    peer.0.wait().unwrap();
    assert!(observer.peers().unwrap().is_empty());
}
