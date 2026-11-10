use super::*;

#[test]
fn only_loopback_http_addresses_are_started() {
    assert_eq!(
        local_address("http://127.0.0.1:11434"),
        Some(("127.0.0.1".into(), 11434))
    );
    assert_eq!(
        local_address("http://localhost/v1"),
        Some(("localhost".into(), 11434))
    );
    assert_eq!(
        local_address("http://[::1]:8080"),
        Some(("[::1]".into(), 8080))
    );
    assert_eq!(local_address("https://127.0.0.1:11434"), None);
    assert_eq!(local_address("http://192.168.1.4:11434"), None);
    assert_eq!(local_address("http://ollama.example.com"), None);
}

#[cfg(unix)]
mod lifecycle {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// A stand-in server: `spawn` launches a real sleeping process group and makes it reachable.
    #[derive(Default)]
    struct Fake {
        up: Arc<AtomicBool>,
        spawns: AtomicUsize,
        /// Spawned processes never become reachable.
        silent: bool,
    }

    impl Host for Fake {
        fn reachable(&self) -> bool {
            self.up.load(Ordering::SeqCst)
        }
        fn spawn(&self, _dir: &Path) -> Result<ProcId> {
            self.spawns.fetch_add(1, Ordering::SeqCst);
            let id = sleeper();
            if !self.silent {
                self.up.store(true, Ordering::SeqCst);
            }
            Ok(id)
        }
    }

    /// Spawned stand-ins stay owned here so they are reaped with the test process.
    static CHILDREN: std::sync::Mutex<Vec<std::process::Child>> = std::sync::Mutex::new(Vec::new());

    fn sleeper() -> ProcId {
        let child = process::spawn_group(Command::new("sleep").arg("30")).unwrap();
        let id = ProcId::of(child.id() as i32).unwrap();
        CHILDREN.lock().unwrap().push(child);
        id
    }

    fn wait_dead(id: ProcId) -> bool {
        (0..200).any(|_| {
            if id.alive() {
                std::thread::sleep(Duration::from_millis(10));
                false
            } else {
                true
            }
        })
    }

    #[test]
    fn a_running_server_is_reused_and_never_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let host = Fake::default();
        host.up.store(true, Ordering::SeqCst);
        let me = ProcId::current().unwrap();
        assert_eq!(
            ensure_with(dir.path(), me, &host, Duration::ZERO).unwrap(),
            Ready::Running
        );
        assert_eq!(host.spawns.load(Ordering::SeqCst), 0);
        assert!(!release(dir.path(), me).unwrap());
    }

    #[test]
    fn one_server_is_shared_and_stopped_after_the_last_live_client() {
        let dir = tempfile::tempdir().unwrap();
        let host = Fake::default();
        let first = sleeper();
        let second = sleeper();
        assert_eq!(
            ensure_with(dir.path(), first, &host, Duration::from_secs(1)).unwrap(),
            Ready::Started
        );
        assert_eq!(
            ensure_with(dir.path(), second, &host, Duration::from_secs(1)).unwrap(),
            Ready::Running
        );
        assert_eq!(host.spawns.load(Ordering::SeqCst), 1);
        let server = read_server(dir.path()).unwrap();
        assert!(!release(dir.path(), first).unwrap(), "second still uses it");
        assert!(server.alive());
        assert!(release(dir.path(), second).unwrap());
        assert!(wait_dead(server), "the server Dial started is stopped");
        assert!(read_server(dir.path()).is_none());
        for client in [first, second] {
            process::kill_group(client.pid as u32).unwrap();
        }
    }

    #[test]
    fn concurrent_callers_start_a_single_server() {
        let dir = Arc::new(tempfile::tempdir().unwrap());
        let host = Arc::new(Fake::default());
        let clients: Vec<ProcId> = (0..4).map(|_| sleeper()).collect();
        let handles: Vec<_> = clients
            .iter()
            .map(|&client| {
                let (dir, host) = (dir.clone(), host.clone());
                std::thread::spawn(move || {
                    ensure_with(dir.path(), client, &*host, Duration::from_secs(1)).unwrap()
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(host.spawns.load(Ordering::SeqCst), 1);
        let server = read_server(dir.path()).unwrap();
        for &client in &clients {
            release(dir.path(), client).unwrap();
            process::kill_group(client.pid as u32).unwrap();
        }
        assert!(wait_dead(server));
    }

    #[test]
    fn a_dead_client_lease_does_not_keep_the_server_alive() {
        let dir = tempfile::tempdir().unwrap();
        let host = Fake::default();
        let crashed = sleeper();
        let me = ProcId::current().unwrap();
        ensure_with(dir.path(), crashed, &host, Duration::from_secs(1)).unwrap();
        ensure_with(dir.path(), me, &host, Duration::ZERO).unwrap();
        process::kill_group(crashed.pid as u32).unwrap();
        assert!(wait_dead(crashed));
        let server = read_server(dir.path()).unwrap();
        assert!(release(dir.path(), me).unwrap());
        assert!(wait_dead(server));
    }

    #[test]
    fn a_server_another_client_is_starting_is_awaited_not_spawned_again() {
        let dir = tempfile::tempdir().unwrap();
        let host = Fake {
            silent: true,
            ..Fake::default()
        };
        let first = sleeper();
        assert_eq!(
            ensure_with(dir.path(), first, &host, Duration::ZERO).unwrap(),
            Ready::Starting
        );
        let me = ProcId::current().unwrap();
        assert_eq!(
            ensure_with(dir.path(), me, &host, Duration::ZERO).unwrap(),
            Ready::Starting
        );
        assert_eq!(host.spawns.load(Ordering::SeqCst), 1);
        let server = read_server(dir.path()).unwrap();
        process::kill_group(first.pid as u32).unwrap();
        assert!(wait_dead(first));
        assert!(release(dir.path(), me).unwrap());
        assert!(wait_dead(server));
    }

    #[test]
    fn a_dead_server_record_is_replaced_by_one_new_start() {
        let dir = tempfile::tempdir().unwrap();
        let host = Fake {
            silent: true,
            ..Fake::default()
        };
        let me = ProcId::current().unwrap();
        ensure_with(dir.path(), me, &host, Duration::ZERO).unwrap();
        let server = read_server(dir.path()).unwrap();
        process::kill_group(server.pid as u32).unwrap();
        assert!(wait_dead(server));
        // The dead record is replaced by a new start rather than awaited forever.
        ensure_with(dir.path(), me, &host, Duration::ZERO).unwrap();
        assert_eq!(host.spawns.load(Ordering::SeqCst), 2);
        let replacement = read_server(dir.path()).unwrap();
        assert!(release(dir.path(), me).unwrap());
        assert!(wait_dead(replacement));
    }
}
