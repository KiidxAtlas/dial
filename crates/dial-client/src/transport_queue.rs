//! Count- and byte-bounded transport handoff. An oversized frame occupies the queue alone.
use std::collections::VecDeque;
use std::sync::mpsc::{RecvTimeoutError, SendError, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug)]
struct State<T> {
    queue: VecDeque<(T, usize)>,
    bytes: usize,
    senders: usize,
    receiver: bool,
}

#[derive(Debug)]
struct Shared<T> {
    state: Mutex<State<T>>,
    changed: Condvar,
    count: usize,
    bytes: usize,
    weight: fn(&T) -> usize,
}

#[derive(Debug)]
pub(super) struct Sender<T>(Arc<Shared<T>>);
#[derive(Debug)]
pub(super) struct Receiver<T>(Arc<Shared<T>>);

pub(super) fn channel<T>(
    count: usize,
    bytes: usize,
    weight: fn(&T) -> usize,
) -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            queue: VecDeque::new(),
            bytes: 0,
            senders: 1,
            receiver: true,
        }),
        changed: Condvar::new(),
        count,
        bytes,
        weight,
    });
    (Sender(shared.clone()), Receiver(shared))
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.0.state.lock().unwrap().senders += 1;
        Self(self.0.clone())
    }
}
impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().senders -= 1;
        self.0.changed.notify_all();
    }
}
impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.receiver = false;
        state.queue.clear();
        self.0.changed.notify_all();
    }
}
impl<T> Sender<T> {
    pub(super) fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        let mut state = self.0.state.lock().unwrap();
        if !state.receiver {
            return Err(TrySendError::Disconnected(value));
        }
        let bytes = (self.0.weight)(&value).min(self.0.bytes);
        if state.queue.len() >= self.0.count || state.bytes + bytes > self.0.bytes {
            return Err(TrySendError::Full(value));
        }
        state.bytes += bytes;
        state.queue.push_back((value, bytes));
        self.0.changed.notify_all();
        Ok(())
    }

    pub(super) fn send(&self, value: T) -> Result<(), SendError<T>> {
        let mut state = self.0.state.lock().unwrap();
        let bytes = (self.0.weight)(&value).min(self.0.bytes);
        while state.receiver
            && (state.queue.len() >= self.0.count || state.bytes + bytes > self.0.bytes)
        {
            state = self.0.changed.wait(state).unwrap();
        }
        if !state.receiver {
            return Err(SendError(value));
        }
        state.bytes += bytes;
        state.queue.push_back((value, bytes));
        self.0.changed.notify_all();
        Ok(())
    }
}
impl<T> Receiver<T> {
    pub(super) fn recv(&self) -> Result<T, RecvTimeoutError> {
        let mut state = self.0.state.lock().unwrap();
        loop {
            if let Some((value, bytes)) = state.queue.pop_front() {
                state.bytes -= bytes;
                self.0.changed.notify_all();
                return Ok(value);
            }
            if state.senders == 0 {
                return Err(RecvTimeoutError::Disconnected);
            }
            state = self.0.changed.wait(state).unwrap();
        }
    }
    pub(super) fn recv_timeout(&self, timeout: Duration) -> Result<T, RecvTimeoutError> {
        let deadline = Instant::now() + timeout;
        let mut state = self.0.state.lock().unwrap();
        loop {
            if let Some((value, bytes)) = state.queue.pop_front() {
                state.bytes -= bytes;
                self.0.changed.notify_all();
                return Ok(value);
            }
            if state.senders == 0 {
                return Err(RecvTimeoutError::Disconnected);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(RecvTimeoutError::Timeout);
            }
            state = self.0.changed.wait_timeout(state, left).unwrap().0;
        }
    }
}

#[cfg(test)]
mod tests;
