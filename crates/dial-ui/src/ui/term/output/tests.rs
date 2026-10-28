use std::{
    io::{self, Write},
    sync::{Arc, Condvar, Mutex, mpsc},
    time::{Duration, Instant},
};

use ratatui::text::Line;

use super::*;
use crate::ui::term::{InlineTerminal, Viewport};

/// A terminal that accepts nothing until opened, then records what it received.
#[derive(Clone, Default)]
struct Gate {
    open: Arc<(Mutex<bool>, Condvar)>,
    seen: Arc<Mutex<Vec<u8>>>,
    fail: bool,
}

impl Gate {
    fn closed() -> Self {
        Self::default()
    }

    fn open(&self) {
        let (lock, changed) = &*self.open;
        *lock.lock().unwrap() = true;
        changed.notify_all();
    }

    fn seen(&self) -> Vec<u8> {
        self.seen.lock().unwrap().clone()
    }
}

impl Write for Gate {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let (lock, changed) = &*self.open;
        let mut open = lock.lock().unwrap();
        while !*open {
            open = changed.wait(open).unwrap();
        }
        if self.fail {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "reader gone"));
        }
        self.seen.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn open_gate() -> Gate {
    let gate = Gate::default();
    gate.open();
    gate
}

#[test]
fn frames_reach_the_terminal_in_push_order() {
    let gate = open_gate();
    let mut sink = Sink::queued(Box::new(gate.clone())).expect("writer thread");
    sink.write_all(b"a").expect("push");
    sink.write_all(b"bb").expect("push");
    sink.write_all(b"ccc").expect("push");
    sink.drain(Duration::from_secs(5)).expect("drain");
    assert_eq!(gate.seen(), b"abbccc");
}

#[test]
fn a_blocked_terminal_accepts_small_writes_but_reports_backlog() {
    let gate = Gate::closed();
    let mut sink = Sink::queued(Box::new(gate.clone())).expect("writer thread");
    let frame = vec![b'x'; BACKLOG_LIMIT + 1];
    sink.write_all(&frame)
        .expect("push returns without waiting");
    assert!(sink.backlogged());
    let stalled = sink.drain(Duration::from_millis(50)).expect_err("stalled");
    assert_eq!(stalled.kind(), io::ErrorKind::TimedOut);

    gate.open();
    sink.drain(Duration::from_secs(5)).expect("drain");
    assert!(!sink.backlogged());
    assert_eq!(gate.seen(), frame);
}

#[test]
fn a_full_queue_refuses_writes_without_waiting() {
    const CHUNK: usize = 1024 * 1024;
    let gate = Gate::closed();
    let mut sink = Sink::queued(Box::new(gate.clone())).expect("writer thread");
    let chunk = vec![b'y'; CHUNK];
    // Eight chunks fill the limit exactly; the writer holds the first one in flight.
    for _ in 0..8 {
        assert!(sink.try_write(&chunk).expect("admitted"));
    }
    let started = Instant::now();
    assert!(!sink.try_write(&chunk).expect("refused, not failed"));
    let err = sink.write(&chunk).expect_err("backlogged write");
    assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
    assert!(started.elapsed() < Duration::from_secs(1));

    gate.open();
    sink.drain(Duration::from_secs(5)).expect("drain");
    assert!(sink.try_write(&chunk).expect("admitted after drain"));
    sink.drain(Duration::from_secs(5)).expect("drain");
    assert_eq!(gate.seen().len(), CHUNK * 9);
}

#[test]
fn an_oversized_chunk_is_admitted_only_into_an_empty_queue() {
    let gate = Gate::closed();
    let mut sink = Sink::queued(Box::new(gate.clone())).expect("writer thread");
    let oversized = vec![b'z'; HARD_LIMIT + 1];
    assert!(sink.try_write(b"a").expect("admitted"));
    assert!(
        !sink
            .try_write(&oversized)
            .expect("refused behind queued bytes")
    );
    gate.open();
    sink.drain(Duration::from_secs(5)).expect("drain");
    assert!(
        sink.try_write(&oversized)
            .expect("admitted into an empty queue")
    );
    sink.drain(Duration::from_secs(5)).expect("drain");
    assert_eq!(gate.seen().len(), 1 + oversized.len());
}

#[test]
fn a_write_failure_is_sticky_for_drain_and_later_writes() {
    let gate = Gate {
        fail: true,
        ..open_gate()
    };
    let mut sink = Sink::queued(Box::new(gate)).expect("writer thread");
    sink.write_all(b"x").expect("push");
    assert!(sink.drain(Duration::from_secs(5)).is_err());
    assert!(sink.write(b"y").is_err());
}

#[test]
fn dropping_the_sink_lets_the_writer_finish_queued_bytes() {
    let gate = open_gate();
    let mut sink = Sink::queued(Box::new(gate.clone())).expect("writer thread");
    sink.write_all(b"abc").expect("push");
    drop(sink);
    for _ in 0..500 {
        if gate.seen() == b"abc" {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(gate.seen(), b"abc");
}

#[test]
fn history_rows_reach_the_terminal_in_order_through_the_queue() {
    let gate = Gate::closed();
    let mut term = InlineTerminal::build(
        Sink::queued(Box::new(gate.clone())).expect("writer thread"),
        (40, 10),
        false,
        false,
    );
    term.commit(vec![Line::from("first row")]);
    term.commit(vec![Line::from("second row")]);
    gate.open();
    term.leave();
    term.flush_output().expect("flush");
    let text = String::from_utf8_lossy(&gate.seen()).into_owned();
    let first = text.find("first row").expect("first row written");
    let second = text.find("second row").expect("second row written");
    assert!(first < second, "history must keep commit order: {text:?}");
}

#[test]
fn a_stalled_terminal_defers_a_forced_paint_and_retries_it_exactly_once() {
    let gate = Gate::closed();
    let (deferred_tx, deferred_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel::<()>();
    let (done_tx, done_rx) = mpsc::channel();
    let worker_gate = gate.clone();
    let worker = thread::spawn(move || {
        let mut term = InlineTerminal::build(
            Sink::queued(Box::new(worker_gate)).expect("writer thread"),
            (40, 10),
            false,
            false,
        );
        // Fill the queue to the limit; the stalled writer holds this chunk in flight.
        assert!(
            term.out
                .try_write(&vec![b'y'; HARD_LIMIT])
                .expect("filler admitted")
        );
        term.commit(vec![
            Line::from("row-00000"),
            Line::from("row-00001"),
            Line::from("row-00002"),
        ]);
        let live = [Line::raw("composer")];
        let viewport = Viewport {
            rows: &live,
            cursor: None,
            title: None,
            signals: None,
        };
        let pending = term.history_rows_pending();
        let written = term.history_rows_written();
        // The queue is full: the paint is refused at once and records nothing as written.
        let outcome = term.paint(&viewport).map_err(|error| error.to_string());
        deferred_tx
            .send((
                outcome,
                pending,
                written,
                term.history_rows_pending(),
                term.history_rows_written(),
            ))
            .expect("report the deferred paint");
        resume_rx.recv().expect("resume after the deferral");
        term.flush_output().expect("flush the filler");
        term.paint(&viewport).expect("retry");
        done_tx
            .send((term.history_rows_pending(), term.history_rows_written()))
            .expect("report the retry");
        term.leave();
        term.flush_output().expect("flush teardown");
    });

    // Bounded: a paint that waited on the stalled terminal would miss this deadline.
    let (outcome, pending, written, pending_after, written_after) = deferred_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("the paint returned while the terminal was stalled");
    assert!(
        outcome.is_ok(),
        "a deferred paint is not an error: {outcome:?}"
    );
    assert_eq!(pending_after, pending);
    assert_eq!(written_after, written);
    assert_eq!(written, 0);

    gate.open();
    resume_tx.send(()).expect("resume the worker");
    let (pending_done, written_done) = done_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the retry lands");
    assert_eq!(pending_done, 0);
    assert_eq!(written_done, 3);
    worker.join().expect("worker");
    let text = String::from_utf8_lossy(&gate.seen()).into_owned();
    assert_eq!(text.matches("row-").count(), 3);
    let first = text.find("row-00000").expect("first row written");
    let last = text.find("row-00002").expect("last row written");
    assert!(first < last, "history must keep commit order");
}

#[test]
fn a_frame_over_the_cap_is_rejected_at_once() {
    let gate = Gate::closed();
    let mut sink = Sink::queued(Box::new(gate.clone())).expect("writer thread");
    let started = Instant::now();
    let err = sink
        .try_write(&vec![b'z'; MAX_FRAME + 1])
        .expect_err("over the cap");
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    assert!(started.elapsed() < Duration::from_millis(500));
    gate.open();
}
