use super::*;
use std::process::Command;

#[test]
fn watcher_signals_new_file_within_bounded_wait() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().canonicalize().expect("canonical temp dir");
    let init = Command::new("git")
        .arg("init")
        .arg("-q")
        .current_dir(&root)
        .status()
        .expect("run git init");
    assert!(init.success(), "git init failed");

    let watch = Watch::start(&root).expect("watch starts in a git repo");
    std::fs::write(root.join("new.txt"), "hello\n").expect("create file");

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut signalled = false;
    while Instant::now() < deadline {
        if watch.poll() {
            signalled = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(signalled, "no change signal for a new file within 2s");
}

#[test]
fn debounce_waits_for_quiet_period() {
    let (tx, rx) = mpsc::channel();
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().canonicalize().expect("canonical temp dir");
    let mut builder = GitignoreBuilder::new(&root);
    let _ = builder.add(root.join(".gitignore"));
    let filter = Filter {
        root: root.clone(),
        gitdir: root.join(".git"),
        ignore: builder.build().expect("empty ignore set"),
    };
    assert!(filter.relevant(&root.join("src.rs")));
    assert!(!filter.relevant(&root.join(".git").join("objects").join("ab")));
    assert!(filter.relevant(&root.join(".git").join("index")));
    assert!(filter.relevant(&root.join(".git").join("refs").join("heads").join("main")));

    let start = Instant::now();
    let mut watch = Watch {
        _watcher: notify::recommended_watcher(|_: notify::Result<notify::Event>| {})
            .expect("watcher"),
        rx,
        first: None,
        last: None,
    };
    tx.send(()).expect("send");
    assert!(!watch.take_ready(start), "must wait for the quiet period");
    assert!(!watch.take_ready(start + Duration::from_millis(100)));
    assert!(watch.take_ready(start + QUIET + Duration::from_millis(1)));
    assert!(!watch.take_ready(start + QUIET + Duration::from_millis(2)));
}
