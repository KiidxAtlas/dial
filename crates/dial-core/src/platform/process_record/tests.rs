use super::*;

fn sample_spec() -> SpawnSpec {
    SpawnSpec {
        program: PathBuf::from("/opt/server/bin/python"),
        args: vec!["-m".into(), "server".into(), "--port=8001".into()],
        cwd: PathBuf::from("/work/project"),
        env_allow: vec!["PATH".into(), "HOME".into()],
        env: vec![("DIAL_MODEL_ENGINE".into(), "mlx-lm".into())],
    }
}

#[test]
fn spawn_spec_round_trips_through_the_record_file() {
    let home = tempfile::tempdir().unwrap();
    let log = Log::create(home.path()).unwrap();
    let mut record = Record::new(
        home.path(),
        4242,
        "models",
        "test-model",
        Path::new("project"),
        Path::new("state"),
        "loading",
    )
    .unwrap();
    record.set_spawn(&sample_spec(), &log).unwrap();
    let loaded = load(home.path(), 4242).unwrap().unwrap();
    assert_eq!(spawn_spec(&loaded).unwrap(), Some(sample_spec()));
    assert_eq!(loaded["log"], json!(log.path()));
    let log_path = log.path().to_path_buf();
    drop(record);
    assert!(!record_path(home.path(), 4242).exists());
    assert!(!log_path.exists());
}

#[test]
fn records_without_spawn_spec_report_none_and_missing_records_load_as_none() {
    let home = tempfile::tempdir().unwrap();
    let _record = Record::new(
        home.path(),
        4243,
        "tools",
        "",
        Path::new("project"),
        Path::new("state"),
        "running",
    )
    .unwrap();
    let loaded = load(home.path(), 4243).unwrap().unwrap();
    assert_eq!(spawn_spec(&loaded).unwrap(), None);
    assert!(load(home.path(), 99_999).unwrap().is_none());
}

#[test]
fn detached_records_survive_their_handle_and_point_at_init() {
    let home = tempfile::tempdir().unwrap();
    let record = Record::detached(
        home.path(),
        4244,
        "models",
        "m",
        Path::new("project"),
        Path::new("state"),
        "loading",
    )
    .unwrap();
    record.detach();
    let loaded = load(home.path(), 4244).unwrap().unwrap();
    assert_eq!(loaded["parent_pid"], 1);
    fs::remove_file(record_path(home.path(), 4244)).unwrap();
}

#[test]
fn read_tail_returns_only_the_bounded_tail_at_a_line_boundary() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("out.log");
    let mut text = String::new();
    for line in 0..2000 {
        text.push_str(&format!("line {line:04}\n"));
    }
    fs::write(&path, &text).unwrap();
    let tail = read_tail(&path, 100).unwrap();
    assert!(tail.len() <= 100);
    assert!(tail.ends_with("line 1999\n"));
    assert!(tail.starts_with("line "));
    assert_eq!(read_tail(&path, 1 << 20).unwrap(), text);
}

/// A model server outliving the Dial process that started it (SIGKILL, an unhandled SIGTERM,
/// a crash) holds its whole model in memory. Only servers whose recorded owner is gone stop;
/// servers of a live Dial process and detached `ps restart` servers keep running.
#[cfg(unix)]
#[test]
fn abandoned_model_servers_stop_but_owned_and_detached_ones_keep_running() {
    use std::process::Command;
    let home = tempfile::tempdir().unwrap();
    let server = || {
        crate::platform::process::spawn_group(Command::new("sleep").arg("60").stdin(Stdio::null()))
            .unwrap()
    };
    let mut gone_owner = Command::new("true").spawn().unwrap();
    let dead = gone_owner.id();
    gone_owner.wait().unwrap();
    let (mut abandoned, mut owned, mut detached) = (server(), server(), server());
    let record = |child: &std::process::Child, parent: u32, persist: bool| {
        let record = Record::create(
            home.path(),
            child.id(),
            parent,
            "models",
            "m",
            Path::new("project"),
            Path::new("state"),
            "loaded",
            persist,
        )
        .unwrap();
        record.detach();
    };
    record(&abandoned, dead, false);
    record(&owned, std::process::id(), false);
    record(&detached, 1, true);

    assert_eq!(stop_abandoned(home.path(), "models"), 1);
    let exited =
        crate::platform::process::wait_exited(&abandoned, std::time::Duration::from_secs(10));
    assert!(exited.unwrap(), "the abandoned server must stop");
    assert!(!record_path(home.path(), abandoned.id()).exists());
    for child in [&mut owned, &mut detached] {
        assert!(
            child.try_wait().unwrap().is_none(),
            "a live owner's or detached server keeps running"
        );
        assert!(record_path(home.path(), child.id()).exists());
    }
    for child in [&mut abandoned, &mut owned, &mut detached] {
        let _ = crate::platform::process::kill_group(child.id());
        let _ = child.wait();
    }
}
