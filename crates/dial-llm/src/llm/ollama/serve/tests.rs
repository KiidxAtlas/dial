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
        fn reachable(&self, _timeout: Duration) -> bool {
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

    /// Waits like a deadline-free caller: `wait` from now, no cancellation.
    fn ensure_within(dir: &Path, me: ProcId, host: &dyn Host, wait: Duration) -> Result<Ready> {
        ensure_with(dir, me, host, Instant::now() + wait, None)
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
            ensure_within(dir.path(), me, &host, QUICK).unwrap(),
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
            ensure_within(dir.path(), first, &host, Duration::from_secs(1)).unwrap(),
            Ready::Started
        );
        assert_eq!(
            ensure_within(dir.path(), second, &host, Duration::from_secs(1)).unwrap(),
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
                    ensure_within(dir.path(), client, &*host, Duration::from_secs(1)).unwrap()
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
        ensure_within(dir.path(), crashed, &host, Duration::from_secs(1)).unwrap();
        ensure_within(dir.path(), me, &host, QUICK).unwrap();
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
            ensure_within(dir.path(), first, &host, QUICK).unwrap(),
            Ready::Starting
        );
        let me = ProcId::current().unwrap();
        assert_eq!(
            ensure_within(dir.path(), me, &host, QUICK).unwrap(),
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
        ensure_within(dir.path(), me, &host, QUICK).unwrap();
        let server = read_server(dir.path()).unwrap();
        process::kill_group(server.pid as u32).unwrap();
        assert!(wait_dead(server));
        // The dead record is replaced by a new start rather than awaited forever.
        ensure_within(dir.path(), me, &host, QUICK).unwrap();
        assert_eq!(host.spawns.load(Ordering::SeqCst), 2);
        let replacement = read_server(dir.path()).unwrap();
        assert!(release(dir.path(), me).unwrap());
        assert!(wait_dead(replacement));
    }

    /// A wait short enough for tests, long enough for one caller to lock, record and spawn.
    const QUICK: Duration = Duration::from_millis(200);

    /// Runs `work` on its own thread and fails the test rather than hanging if it never returns.
    fn within<T: Send + 'static>(limit: Duration, work: impl FnOnce() -> T + Send + 'static) -> T {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(work());
        });
        receiver
            .recv_timeout(limit)
            .expect("the call did not return within its deadline")
    }

    #[test]
    fn a_held_state_lock_is_waited_on_only_until_the_deadline() {
        let dir = Arc::new(tempfile::tempdir().unwrap());
        fs::create_dir_all(dir.path().join(LEASES)).unwrap();
        // Another Dial process holds the state lock, for example while it spawns.
        let holder = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(dir.path().join(LOCK_FILE))
            .unwrap();
        holder.lock().unwrap();
        let host = Arc::new(Fake::default());
        let me = ProcId::current().unwrap();
        let started = Instant::now();
        let (waiter_dir, waiter_host) = (dir.clone(), host.clone());
        let ready = within(Duration::from_secs(10), move || {
            ensure_within(
                waiter_dir.path(),
                me,
                &*waiter_host,
                Duration::from_millis(300),
            )
        });
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(ready.unwrap(), Ready::Starting);
        assert_eq!(
            host.spawns.load(Ordering::SeqCst),
            0,
            "no spawn without the lock"
        );
        drop(holder);
    }

    #[test]
    fn a_client_polling_a_slow_server_does_not_block_other_clients() {
        let dir = Arc::new(tempfile::tempdir().unwrap());
        let host = Arc::new(Fake {
            silent: true,
            ..Fake::default()
        });
        let first = sleeper();
        let (slow_dir, slow_host) = (dir.clone(), host.clone());
        let slow = std::thread::spawn(move || {
            ensure_within(slow_dir.path(), first, &*slow_host, Duration::from_secs(3))
        });
        // Let the first caller spawn and enter its readiness wait.
        while host.spawns.load(Ordering::SeqCst) == 0 {
            std::thread::sleep(Duration::from_millis(5));
        }
        let second = sleeper();
        let started = Instant::now();
        assert_eq!(
            ensure_within(dir.path(), second, &*host, QUICK).unwrap(),
            Ready::Starting
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the lock is not held while polling: {:?}",
            started.elapsed()
        );
        assert_eq!(
            host.spawns.load(Ordering::SeqCst),
            1,
            "still a single spawn"
        );
        assert_eq!(slow.join().unwrap().unwrap(), Ready::Starting);
        let server = read_server(dir.path()).unwrap();
        for client in [first, second] {
            release(dir.path(), client).unwrap();
            process::kill_group(client.pid as u32).unwrap();
        }
        assert!(wait_dead(server));
    }

    #[test]
    fn cancellation_stops_every_wait_without_forgetting_the_started_server() {
        let dir = tempfile::tempdir().unwrap();
        let host = Fake {
            silent: true,
            ..Fake::default()
        };
        let me = ProcId::current().unwrap();
        let cancel = AtomicBool::new(true);
        // Cancelled before anything starts: nothing is spawned.
        let error = ensure_with(
            dir.path(),
            me,
            &host,
            Instant::now() + Duration::from_secs(30),
            Some(&cancel),
        )
        .unwrap_err();
        assert!(error.is::<Cancelled>(), "{error:#}");
        assert_eq!(host.spawns.load(Ordering::SeqCst), 0);

        // Cancelled while polling: the start is kept so a later caller waits for it, not respawns.
        cancel.store(false, Ordering::SeqCst);
        let started = Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(200));
                cancel.store(true, Ordering::SeqCst);
            });
            let error = ensure_with(
                dir.path(),
                me,
                &host,
                Instant::now() + Duration::from_secs(30),
                Some(&cancel),
            )
            .unwrap_err();
            assert!(error.is::<Cancelled>(), "{error:#}");
        });
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(host.spawns.load(Ordering::SeqCst), 1);
        let server = read_server(dir.path()).unwrap();
        assert!(server.alive());
        assert!(release(dir.path(), me).unwrap());
        assert!(wait_dead(server));
    }

    #[test]
    fn an_expired_deadline_or_cancellation_takes_no_lock_and_spawns_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let host = Fake::default();
        let me = ProcId::current().unwrap();
        let expired = Instant::now();
        std::thread::sleep(Duration::from_millis(5));
        // The lock is free: an expired call must not take it, record a lease, or spawn.
        assert_eq!(
            ensure_with(dir.path(), me, &host, expired, None).unwrap(),
            Ready::Starting
        );
        assert_eq!(host.spawns.load(Ordering::SeqCst), 0, "nothing is spawned");
        assert!(
            live_leases(dir.path()).unwrap_or_default().is_empty(),
            "no lease is recorded"
        );
        let cancel = AtomicBool::new(true);
        let error = ensure_with(
            dir.path(),
            me,
            &host,
            Instant::now() + Duration::from_secs(30),
            Some(&cancel),
        )
        .unwrap_err();
        assert!(error.is::<Cancelled>(), "{error:#}");
        assert_eq!(host.spawns.load(Ordering::SeqCst), 0, "nothing is spawned");
        assert!(
            live_leases(dir.path()).unwrap_or_default().is_empty(),
            "no lease is recorded"
        );
    }

    #[test]
    fn a_readiness_wait_ends_within_the_deadline_plus_one_bounded_probe() {
        let dir = tempfile::tempdir().unwrap();
        let host = Fake {
            silent: true,
            ..Fake::default()
        };
        let me = ProcId::current().unwrap();
        let wait = Duration::from_millis(300);
        let started = Instant::now();
        assert_eq!(
            ensure_within(dir.path(), me, &host, wait).unwrap(),
            Ready::Starting
        );
        // The last probe may run past the deadline, by at most its minimum timeout.
        assert!(
            started.elapsed() < wait + MIN_PROBE + Duration::from_millis(500),
            "{:?}",
            started.elapsed()
        );
        let server = read_server(dir.path()).unwrap();
        assert!(release(dir.path(), me).unwrap());
        assert!(wait_dead(server));
    }
}
