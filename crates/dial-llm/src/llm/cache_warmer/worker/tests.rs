use super::*;
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;

/// Replays a fake provider records: the `max_tokens` of each call, returning `read` cache tokens.
struct Fake {
    calls: Arc<Mutex<Vec<u32>>>,
    read: Option<u64>,
}

impl Replayer for Fake {
    fn replay(&mut self, chat: &Chat, _scope: Option<&Scope>) -> Result<Option<u64>> {
        self.calls.lock().unwrap().push(chat.max_tokens);
        Ok(self.read)
    }
}

fn pricing() -> TokenPricing {
    TokenPricing {
        prompt: 3.0 / 1_000_000.0,
        completion: 15.0 / 1_000_000.0,
        cache_read: Some(0.3 / 1_000_000.0),
        cache_write: Some(3.75 / 1_000_000.0),
    }
}

fn chat() -> Chat {
    Chat {
        messages: vec![json!({"role": "user", "content": "hi"})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 4096,
        seed: None,
    }
}

struct Harness {
    handle: Handle,
    now: Arc<Mutex<Instant>>,
    calls: Arc<Mutex<Vec<u32>>>,
    start: Instant,
}

fn harness(mode: Mode, read: Option<u64>) -> Harness {
    let start = Instant::now();
    let now = Arc::new(Mutex::new(start));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let factory_calls = calls.clone();
    let factory: Factory = Box::new(move |_model| {
        Ok(Box::new(Fake {
            calls: factory_calls.clone(),
            read,
        }) as Box<dyn Replayer>)
    });
    let clock_now = now.clone();
    let clock: Clock = Box::new(move || *clock_now.lock().unwrap());
    let handle = Handle::spawn(mode, factory, clock).expect("worker thread spawns");
    Harness {
        handle,
        now,
        calls,
        start,
    }
}

impl Harness {
    fn at(&self, secs: u64) {
        *self.now.lock().unwrap() = self.start + Duration::from_secs(secs);
    }

    fn settle(&self, model: &str) {
        assert!(self.handle.send(Event::Settled {
            chat: chat(),
            model: model.into(),
            prompt_tokens: 1_000_000,
            pricing: Some(pricing()),
            tier: Tier::Short,
            scope: None,
        }));
        // Give the worker one poll cycle to apply the arm at the current clock reading.
        thread::sleep(Duration::from_millis(200));
    }

    fn sync(&self) {
        thread::sleep(Duration::from_millis(200));
    }

    fn calls(&self) -> Vec<u32> {
        self.calls.lock().unwrap().clone()
    }
}

#[test]
fn replay_fires_once_at_due_time_with_one_output_token() {
    let h = harness(Mode::Idle, Some(1_000));
    h.settle("claude");
    h.at(239);
    h.sync();
    assert!(h.calls().is_empty());
    h.at(240);
    h.sync();
    assert_eq!(h.calls(), vec![1]);
    assert!(h.handle.drain().contains(&Report::Replayed {
        cache_read_tokens: Some(1_000),
    }));
}

#[test]
fn context_change_disarms_before_the_next_replay() {
    let h = harness(Mode::Idle, Some(1_000));
    h.settle("claude");
    assert!(h.handle.send(Event::ContextChanged));
    h.sync();
    h.at(240);
    h.sync();
    assert!(h.calls().is_empty());
    assert!(h.handle.drain().contains(&Report::Stopped(Stop::Changed)));
}

#[test]
fn a_replay_that_reads_nothing_stops_warming() {
    let h = harness(Mode::Idle, Some(0));
    h.settle("claude");
    h.at(240);
    h.sync();
    h.at(480);
    h.sync();
    assert_eq!(h.calls(), vec![1]);
    assert!(h.handle.drain().contains(&Report::Stopped(Stop::Miss)));
}

#[test]
fn streaming_warming_stops_when_the_run_ends() {
    let h = harness(Mode::Streaming, Some(1_000));
    h.settle("claude");
    assert!(h.handle.send(Event::RunIdle));
    h.sync();
    h.at(240);
    h.sync();
    assert!(h.calls().is_empty());
    assert!(h.handle.drain().contains(&Report::Stopped(Stop::RunEnded)));
}

#[test]
fn idle_warming_survives_the_run_ending() {
    let h = harness(Mode::Idle, Some(1_000));
    h.settle("claude");
    assert!(h.handle.send(Event::RunIdle));
    h.sync();
    h.at(240);
    h.sync();
    assert_eq!(h.calls(), vec![1]);
}

#[test]
fn a_model_change_disarms() {
    let h = harness(Mode::Idle, Some(1_000));
    h.settle("claude");
    assert!(h.handle.send(Event::ModelChanged("other".into())));
    h.sync();
    h.at(240);
    h.sync();
    assert!(h.calls().is_empty());
}

#[test]
fn turning_warming_off_disarms_and_reports_the_mode_stop() {
    let h = harness(Mode::Idle, Some(1_000));
    h.settle("claude");
    assert!(h.handle.send(Event::ModeChanged(Mode::Off)));
    h.sync();
    h.at(240);
    h.sync();
    assert!(h.calls().is_empty());
    assert!(h.handle.drain().contains(&Report::Stopped(Stop::Mode)));
}

#[test]
fn a_backend_that_cannot_be_built_stops_warming() {
    let factory: Factory = Box::new(|_model| Err(anyhow::anyhow!("no backend")));
    let start = Instant::now();
    let now = Arc::new(Mutex::new(start));
    let clock_now = now.clone();
    let clock: Clock = Box::new(move || *clock_now.lock().unwrap());
    let handle = Handle::spawn(Mode::Idle, factory, clock).expect("worker thread spawns");
    assert!(handle.send(Event::Settled {
        chat: chat(),
        model: "claude".into(),
        prompt_tokens: 1_000_000,
        pricing: Some(pricing()),
        tier: Tier::Short,
        scope: None,
    }));
    thread::sleep(Duration::from_millis(200));
    *now.lock().unwrap() = start + Duration::from_secs(240);
    thread::sleep(Duration::from_millis(300));
    assert!(handle.drain().contains(&Report::Stopped(Stop::Unavailable)));
}

/// Clock reads the scripted run makes before `Shutdown` is queued: loop start, arm, tick, and so on.
const SCRIPTED_READS: usize = 5;

/// Clock readings for one arm at `t0`: loop start, the arm itself, then the due time for every
/// later reading. The first tick after arming therefore sees the prefix already due.
fn due_after_arm() -> Vec<Instant> {
    let t0 = Instant::now();
    vec![t0, t0, t0 + Duration::from_secs(240)]
}

/// Yields `times` in order, then repeats the last reading. When read number `clear_running_at`
/// happens, the running flag is cleared as a queue overflow would. Read `SCRIPTED_READS` queues
/// `Shutdown` so the loop ends.
fn scripted_clock(
    times: Vec<Instant>,
    tx: SyncSender<Event>,
    running: Arc<AtomicBool>,
    clear_running_at: Option<usize>,
) -> Clock {
    let reads = Mutex::new(0usize);
    Box::new(move || {
        let mut n = reads.lock().unwrap();
        let read = *n;
        *n += 1;
        if clear_running_at == Some(read) {
            running.store(false, Ordering::Release);
        }
        if read == SCRIPTED_READS {
            let _ = tx.try_send(Event::Shutdown);
        }
        times[read.min(times.len() - 1)]
    })
}

fn settle_event() -> Event {
    Event::Settled {
        chat: chat(),
        model: "claude".into(),
        prompt_tokens: 1_000_000,
        pricing: Some(pricing()),
        tier: Tier::Short,
        scope: None,
    }
}

/// Runs the worker loop on this thread over `rx` and returns the shared log of replay calls.
fn run_worker(
    mode: Mode,
    rx: Receiver<Event>,
    running: Arc<AtomicBool>,
    reports: SyncSender<Report>,
    clock: Clock,
) -> Arc<Mutex<Vec<u32>>> {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let factory_calls = Arc::clone(&calls);
    let factory: Factory = Box::new(move |_model| {
        Ok(Box::new(Fake {
            calls: Arc::clone(&factory_calls),
            read: Some(1_000),
        }) as Box<dyn Replayer>)
    });
    let worker = Worker {
        mode,
        warmer: Warmer::default(),
        model: None,
        replayer: None,
        scope: None,
        factory,
        clock,
        reports,
        running,
    };
    worker.run(rx);
    calls
}

/// Queues `events` in order, then runs the worker over them with a scripted clock. Returns the
/// replay calls made and the reports produced.
fn run_scripted(
    mode: Mode,
    events: Vec<Event>,
    times: Vec<Instant>,
    clear_running_at: Option<usize>,
) -> (Vec<u32>, Vec<Report>) {
    let (tx, rx) = mpsc::sync_channel(events.len() + 1);
    for event in events {
        tx.try_send(event)
            .expect("queue has room for the scripted events");
    }
    let running = Arc::new(AtomicBool::new(true));
    let clock = scripted_clock(times, tx.clone(), Arc::clone(&running), clear_running_at);
    let (report_tx, report_rx) = mpsc::sync_channel(QUEUE);
    let calls = run_worker(mode, rx, running, report_tx, clock);
    let made = calls.lock().unwrap().clone();
    (made, report_rx.try_iter().collect())
}

/// A handle whose worker is not spawned, so a test runs `Worker::run` over `rx` itself.
struct Detached {
    handle: Handle,
    rx: Receiver<Event>,
    running: Arc<AtomicBool>,
}

fn detached() -> Detached {
    let (tx, rx) = mpsc::sync_channel(QUEUE);
    let (_report_tx, reports) = mpsc::sync_channel(QUEUE);
    let running = Arc::new(AtomicBool::new(true));
    let handle = Handle {
        tx,
        reports,
        running: Arc::clone(&running),
    };
    Detached {
        handle,
        rx,
        running,
    }
}

/// Queues `count` run-end events, asserting each one is accepted.
fn fill(handle: &Handle, count: usize) {
    for _ in 0..count {
        assert!(handle.send(Event::RunIdle), "queue has room");
    }
}

#[test]
fn queued_settle_alone_fires_at_its_due_time() {
    // Control: the scripted clock reaches a due tick, so the invalidation tests below are not
    // passing merely because the worker never ticks.
    let (calls, reports) = run_scripted(Mode::Idle, vec![settle_event()], due_after_arm(), None);
    assert_eq!(calls, vec![1]);
    assert!(reports.contains(&Report::Replayed {
        cache_read_tokens: Some(1_000),
    }));
}

#[test]
fn queued_context_change_is_applied_before_a_due_replay_fires() {
    let (calls, reports) = run_scripted(
        Mode::Idle,
        vec![settle_event(), Event::ContextChanged],
        due_after_arm(),
        None,
    );
    assert!(calls.is_empty(), "stale prefix replayed: {calls:?}");
    assert!(reports.contains(&Report::Stopped(Stop::Changed)));
}

#[test]
fn queued_run_end_stops_a_streaming_prefix_before_it_is_due() {
    let (calls, reports) = run_scripted(
        Mode::Streaming,
        vec![settle_event(), Event::RunIdle],
        due_after_arm(),
        None,
    );
    assert!(calls.is_empty(), "stale prefix replayed: {calls:?}");
    assert!(reports.contains(&Report::Stopped(Stop::RunEnded)));
}

#[test]
fn an_overflow_during_the_run_stops_before_the_due_tick() {
    // The flag clears on the arm reading, after the arm but before the drain and tick check it.
    let (calls, reports) = run_scripted(Mode::Idle, vec![settle_event()], due_after_arm(), Some(1));
    assert!(calls.is_empty(), "replayed after overflow: {calls:?}");
    assert!(reports.contains(&Report::Stopped(Stop::Unavailable)));
}

#[test]
fn a_stop_during_the_due_clock_read_prevents_replay() {
    let (calls, reports) = run_scripted(Mode::Idle, vec![settle_event()], due_after_arm(), Some(2));
    assert!(calls.is_empty(), "replayed after stop: {calls:?}");
    assert!(reports.contains(&Report::Stopped(Stop::Unavailable)));
}

#[test]
fn a_saturated_queue_with_a_rejected_invalidation_never_replays_its_prefix() {
    let d = detached();
    assert!(d.handle.send(settle_event()));
    fill(&d.handle, QUEUE - 1);
    assert!(!d.handle.send(Event::ContextChanged));
    assert!(!d.handle.is_running());
    let (report_tx, report_rx) = mpsc::sync_channel(QUEUE);
    let clock: Clock = Box::new(Instant::now);
    let calls = run_worker(Mode::Idle, d.rx, Arc::clone(&d.running), report_tx, clock);
    assert!(calls.lock().unwrap().is_empty());
    let reports: Vec<Report> = report_rx.try_iter().collect();
    assert!(reports.contains(&Report::Stopped(Stop::Unavailable)));
}

#[test]
fn a_run_guard_that_cannot_queue_its_run_end_stops_the_worker() {
    let d = detached();
    drop(d.handle.run_guard());
    assert!(d.handle.is_running());
    assert!(matches!(d.rx.try_recv(), Ok(Event::RunIdle)));
    fill(&d.handle, QUEUE);
    drop(d.handle.run_guard());
    assert!(!d.handle.is_running());
}

#[test]
fn an_overflowing_send_disables_later_events_even_after_the_queue_drains() {
    let d = detached();
    fill(&d.handle, QUEUE);
    assert!(!d.handle.send(Event::ContextChanged));
    while d.rx.try_recv().is_ok() {}
    assert!(!d.handle.send(settle_event()));
    assert!(!d.handle.is_running());
}

#[test]
fn dropping_the_handle_ends_the_worker_while_a_run_guard_holds_a_sender() {
    // The guard keeps the queue connected, so only the running flag can end this run. Without
    // it the worker would wait on the queue forever.
    let Detached {
        handle,
        rx,
        running,
    } = detached();
    let guard = handle.run_guard();
    assert!(handle.send(settle_event()));
    drop(handle);
    assert!(!running.load(Ordering::Acquire));
    let (report_tx, _report_rx) = mpsc::sync_channel(QUEUE);
    let clock: Clock = Box::new(Instant::now);
    let calls = run_worker(Mode::Idle, rx, running, report_tx, clock);
    assert!(calls.lock().unwrap().is_empty());
    drop(guard);
}

#[test]
fn a_stop_during_backend_construction_prevents_the_paid_request() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let replay_calls = Arc::clone(&calls);
    let running = Arc::new(AtomicBool::new(true));
    let stop = Arc::clone(&running);
    let (reports, _rx) = mpsc::sync_channel(QUEUE);
    let mut worker = Worker {
        mode: Mode::Idle,
        warmer: Warmer::default(),
        model: Some("claude".into()),
        replayer: None,
        scope: None,
        factory: Box::new(move |_| {
            stop.store(false, Ordering::Release);
            Ok(Box::new(Fake {
                calls: Arc::clone(&replay_calls),
                read: Some(1_000),
            }))
        }),
        clock: Box::new(Instant::now),
        reports,
        running,
    };
    worker.fire(&chat());
    assert!(
        calls.lock().unwrap().is_empty(),
        "stopped during backend construction"
    );
}
