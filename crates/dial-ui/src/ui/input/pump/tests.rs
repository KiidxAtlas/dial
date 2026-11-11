use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use super::*;

/// What a scripted source observed.
#[derive(Default)]
struct Probe {
    polls: AtomicUsize,
    reads: AtomicUsize,
    dropped: AtomicUsize,
}

/// Replays `events`; a source with `fail` set reports a lost terminal instead.
struct Scripted {
    events: VecDeque<Event>,
    fail: bool,
    probe: Arc<Probe>,
}

impl Scripted {
    fn boxed(events: Vec<Event>, probe: &Arc<Probe>) -> Box<Self> {
        Box::new(Self {
            events: events.into(),
            fail: false,
            probe: Arc::clone(probe),
        })
    }
}

impl EventSource for Scripted {
    fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
        self.probe.polls.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            return Err(io::Error::other("terminal lost"));
        }
        if !self.events.is_empty() {
            return Ok(true);
        }
        thread::sleep(timeout.min(Duration::from_millis(5)));
        Ok(false)
    }

    fn read(&mut self) -> io::Result<Event> {
        self.probe.reads.fetch_add(1, Ordering::SeqCst);
        self.events
            .pop_front()
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "no scripted event"))
    }
}

impl Drop for Scripted {
    fn drop(&mut self) {
        self.probe.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

fn resize(n: u16) -> Event {
    Event::Resize(n, n)
}

fn pump() -> Arc<Pump> {
    Arc::new(Pump::new())
}

#[test]
fn reader_events_are_delivered_in_order() {
    let pump = pump();
    let probe = Arc::new(Probe::default());
    let idle_probe = Arc::new(Probe::default());
    let mut idle = Scripted::boxed(Vec::new(), &idle_probe);
    pump.start(Scripted::boxed(
        vec![resize(1), resize(2), resize(3)],
        &probe,
    ))
    .expect("start");
    assert_eq!(pump.read_with(idle.as_mut()).expect("read"), resize(1));
    assert_eq!(pump.read_with(idle.as_mut()).expect("read"), resize(2));
    assert_eq!(pump.read_with(idle.as_mut()).expect("read"), resize(3));
    pump.stop();
}

#[test]
fn focus_reports_update_focus_state_and_are_not_delivered() {
    let pump = pump();
    let probe = Arc::new(Probe::default());
    let idle_probe = Arc::new(Probe::default());
    let mut idle = Scripted::boxed(Vec::new(), &idle_probe);
    pump.start(Scripted::boxed(
        vec![Event::FocusLost, resize(1), Event::FocusGained, resize(2)],
        &probe,
    ))
    .expect("start");
    assert_eq!(pump.read_with(idle.as_mut()).expect("read"), resize(1));
    assert_eq!(pump.read_with(idle.as_mut()).expect("read"), resize(2));
    assert!(crate::ui::termcaps::terminal_focused());
    pump.stop();
    // Stopped: a direct read consumes focus reports too.
    let mut direct = Scripted::boxed(vec![Event::FocusLost, resize(3)], &idle_probe);
    assert_eq!(pump.read_with(direct.as_mut()).expect("read"), resize(3));
    assert!(!crate::ui::termcaps::terminal_focused());
    crate::ui::termcaps::set_focus(None);
}

#[test]
fn stop_joins_the_reader_and_queued_events_survive_a_restart() {
    let pump = pump();
    let first = Arc::new(Probe::default());
    let second = Arc::new(Probe::default());
    let idle_probe = Arc::new(Probe::default());
    let mut idle = Scripted::boxed(Vec::new(), &idle_probe);

    pump.start(Scripted::boxed(vec![resize(1)], &first))
        .expect("start");
    assert!(
        pump.poll_with(Duration::from_secs(2), idle.as_mut())
            .expect("poll")
    );
    pump.stop();
    assert_eq!(
        first.dropped.load(Ordering::SeqCst),
        1,
        "stop must drop the reader's source before returning"
    );

    pump.start(Scripted::boxed(vec![resize(2)], &second))
        .expect("restart");
    assert_eq!(pump.read_with(idle.as_mut()).expect("read"), resize(1));
    assert_eq!(pump.read_with(idle.as_mut()).expect("read"), resize(2));
    pump.stop();
}

#[test]
fn while_stopped_calls_fall_back_to_the_direct_source() {
    let pump = pump();
    let idle_probe = Arc::new(Probe::default());
    let mut idle = Scripted::boxed(vec![resize(7)], &idle_probe);
    assert!(
        pump.poll_with(Duration::from_secs(1), idle.as_mut())
            .expect("poll")
    );
    assert_eq!(pump.read_with(idle.as_mut()).expect("read"), resize(7));
    assert_eq!(idle_probe.reads.load(Ordering::SeqCst), 1);
}

#[test]
fn a_running_reader_is_the_only_reader() {
    let pump = pump();
    let probe = Arc::new(Probe::default());
    let idle_probe = Arc::new(Probe::default());
    let mut idle = Scripted::boxed(vec![resize(9)], &idle_probe);
    pump.start(Scripted::boxed(vec![resize(1)], &probe))
        .expect("start");
    assert!(
        pump.poll_with(Duration::from_secs(2), idle.as_mut())
            .expect("poll")
    );
    assert_eq!(pump.read_with(idle.as_mut()).expect("read"), resize(1));
    assert_eq!(
        idle_probe.polls.load(Ordering::SeqCst),
        0,
        "the direct source must not be touched while the reader runs"
    );
    assert_eq!(idle_probe.reads.load(Ordering::SeqCst), 0);
    pump.stop();
}

#[test]
fn a_reader_error_reaches_the_next_poll() {
    let pump = pump();
    let probe = Arc::new(Probe::default());
    let idle_probe = Arc::new(Probe::default());
    let mut idle = Scripted::boxed(Vec::new(), &idle_probe);
    let mut source = Scripted::boxed(Vec::new(), &probe);
    source.fail = true;
    pump.start(source).expect("start");
    assert!(
        pump.poll_with(Duration::from_secs(2), idle.as_mut())
            .is_err()
    );
    pump.stop();
}

#[test]
fn a_full_queue_pauses_the_reader_without_losing_events() {
    let pump = pump();
    let probe = Arc::new(Probe::default());
    let idle_probe = Arc::new(Probe::default());
    let mut idle = Scripted::boxed(Vec::new(), &idle_probe);
    let events = (0..CAPACITY as u16 + 10)
        .map(|n| Event::Resize(n, 0))
        .collect();
    pump.start(Scripted::boxed(events, &probe)).expect("start");
    let mut settled = 0;
    for _ in 0..200 {
        thread::sleep(Duration::from_millis(10));
        if probe.reads.load(Ordering::SeqCst) == CAPACITY {
            settled += 1;
            if settled >= 5 {
                break;
            }
        } else {
            settled = 0;
        }
    }
    assert_eq!(probe.reads.load(Ordering::SeqCst), CAPACITY);
    assert_eq!(
        pump.read_with(idle.as_mut()).expect("read"),
        Event::Resize(0, 0)
    );
    pump.stop();
}

#[test]
fn stopped_poll_preserves_zero_timeout_readiness_and_the_overall_deadline() {
    struct Idle {
        ready_on: Option<usize>,
        polls: Vec<Duration>,
        quiet: std::sync::mpsc::Receiver<()>,
    }
    impl EventSource for Idle {
        fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
            self.polls.push(timeout);
            if self.ready_on == Some(self.polls.len()) {
                return Ok(true);
            }
            assert_eq!(
                self.quiet.recv_timeout(timeout),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            );
            Ok(false)
        }
        fn read(&mut self) -> io::Result<Event> {
            panic!("poll must not read the idle source")
        }
    }
    for (label, timeout, ready_on) in [
        ("zero-ready", Duration::ZERO, Some(1)),
        ("zero-empty", Duration::ZERO, None),
        ("later-slice-ready", Duration::from_secs(1), Some(2)),
        ("whole-deadline", TICK * 3, None),
    ] {
        let (_keep_open, quiet) = std::sync::mpsc::channel();
        let mut idle = Idle {
            ready_on,
            polls: Vec::new(),
            quiet,
        };
        let started = Instant::now();
        assert_eq!(
            pump().poll_with(timeout, &mut idle).expect(label),
            ready_on.is_some(),
            "{label}"
        );
        if ready_on.is_none() {
            assert!(
                started.elapsed() >= timeout,
                "{label}: a false result must not shorten the requested wait"
            );
        }
        assert!(
            !idle.polls.is_empty(),
            "{label}: even a zero timeout checks readiness"
        );
        assert!(
            idle.polls.iter().all(|slice| *slice <= TICK),
            "{label}: unbounded slices {:?}",
            idle.polls
        );
    }
}

#[cfg(unix)]
mod signals;
