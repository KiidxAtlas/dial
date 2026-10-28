use super::*;
use serde_json::json;
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
