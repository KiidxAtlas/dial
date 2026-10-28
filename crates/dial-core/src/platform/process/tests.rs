use super::*;
use std::io::Read;
use std::process::Stdio;
use std::sync::{Arc, Barrier, mpsc};
use std::thread;
use std::time::Duration;

fn piped(program: &str, args: &[&str]) -> Command {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// A child spawned concurrently with a long-lived sibling must not inherit the sibling's pipes:
/// otherwise the short child's stderr reaches EOF only when the sibling exits, which is how an
/// MCP client's stderr drain blocked its `Drop` forever.
#[test]
fn concurrent_group_and_ordinary_spawns_do_not_share_stdio_pipes() {
    const PAIRS: usize = 4;
    const ROUNDS: usize = 40;
    for round in 0..ROUNDS {
        let start = Arc::new(Barrier::new(PAIRS * 2));
        let spawners: Vec<_> = (0..PAIRS * 2)
            .map(|index| {
                let start = start.clone();
                thread::spawn(move || {
                    let mut command = if index % 2 == 0 {
                        piped("/bin/sh", &["-c", "exit 0"])
                    } else {
                        piped("/bin/sleep", &["30"])
                    };
                    start.wait();
                    let child = if (index + round) % 3 == 0 {
                        spawn(&mut command)
                    } else {
                        spawn_group(&mut command)
                    }
                    .unwrap();
                    (index % 2 == 0, child)
                })
            })
            .collect();
        let children: Vec<_> = spawners.into_iter().map(|s| s.join().unwrap()).collect();
        let (done, finished) = mpsc::channel();
        let mut lingering = Vec::new();
        for (short, mut child) in children {
            if short {
                let mut stderr = child.stderr.take().unwrap();
                let done = done.clone();
                thread::spawn(move || {
                    let _ = stderr.read_to_end(&mut Vec::new());
                    let _ = child.wait();
                    let _ = done.send(());
                });
            } else {
                lingering.push(child);
            }
        }
        let all_closed = (0..PAIRS).all(|_| finished.recv_timeout(Duration::from_secs(5)).is_ok());
        for mut child in lingering {
            let _ = kill_group(child.id());
            let _ = child.kill();
            let _ = child.wait();
        }
        assert!(
            all_closed,
            "a short child's stderr stayed open while a concurrently spawned sibling lived"
        );
    }
}
