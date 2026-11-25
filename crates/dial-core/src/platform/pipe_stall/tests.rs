use super::*;
use std::collections::BTreeMap;

#[test]
fn stat_lines_give_the_name_and_process_group() {
    assert_eq!(
        parse_stat("812 (postfix master) S 1 812 812 0 -1 4194560"),
        Some(Stat {
            name: "postfix master".into(),
            group: 812
        })
    );
    assert_eq!(
        parse_stat("9 (a) b)) S 1 40 40 0").map(|stat| stat.group),
        Some(40)
    );
    assert_eq!(parse_stat("9 (tail"), None);
}

#[test]
fn only_write_access_modes_count_as_writers() {
    assert!(opened_for_writing("pos:\t0\nflags:\t01\nmnt_id:\t14\n"));
    assert!(opened_for_writing("pos:\t0\nflags:\t02000002\n"));
    assert!(!opened_for_writing("pos:\t0\nflags:\t02000000\n"));
    assert!(!opened_for_writing("pos:\t0\n"));
}

#[test]
fn a_reader_stalls_only_when_every_writer_left_the_group() {
    let stat = |name: &str, group| Stat {
        name: name.into(),
        group,
    };
    let stats = BTreeMap::from([
        (10, stat("bash", 10)),
        (11, stat("tail", 10)),
        (12, stat("master", 12)),
        (13, stat("postfix", 10)),
    ]);
    let readers = [(11, 500)];
    // Only the daemon holds the pipe: tail can never see end of input.
    let detached = BTreeMap::from([(500, vec![12])]);
    assert_eq!(
        stalled(10, &readers, &detached, &stats),
        vec![PipeStall {
            reader: 11,
            reader_name: "tail".into(),
            writers: vec![(12, "master".into())],
        }]
    );
    // A writer still in the group may yet close it or write more.
    let live = BTreeMap::from([(500, vec![12, 13])]);
    assert!(stalled(10, &readers, &live, &stats).is_empty());
    // No writer at all means end of input is already readable.
    assert!(stalled(10, &readers, &BTreeMap::new(), &stats).is_empty());
    // A writer whose process table entry is unknown is not assumed detached.
    let unknown = BTreeMap::from([(500, vec![99])]);
    assert!(stalled(10, &readers, &unknown, &stats).is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn a_pipe_held_by_a_new_session_is_found_on_linux() {
    use std::os::unix::process::CommandExt;
    use std::time::{Duration, Instant};
    let Ok(mut child) = std::process::Command::new("sh")
        .args(["-c", "setsid sleep 30 | cat"])
        .stdout(std::process::Stdio::null())
        .process_group(0)
        .spawn()
    else {
        return;
    };
    let group = child.id();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut found = Vec::new();
    while found.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        found = detached_pipe_stalls(group);
    }
    for stall in &found {
        for (writer, _) in &stall.writers {
            let _ = crate::platform::process::signal_pid(*writer, 9);
        }
    }
    let _ = crate::platform::process::kill_group(group);
    let _ = child.wait();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].reader_name, "cat");
    assert_eq!(found[0].writers.len(), 1, "{found:?}");
    assert_eq!(found[0].writers[0].1, "sleep");
}
