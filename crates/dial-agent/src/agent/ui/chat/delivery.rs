//! Request-local, byte-bounded delivery. Only the provider's current callback and the
//! controller's current event may own a large value; the intervening queue never does.
use super::ChatEvent;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::time::{Duration, Instant};

const FRAGMENT_BYTES: usize = 16 * 1024;
const QUEUED_FRAGMENTS: usize = 8;
// Each frame has at most FRAGMENT_BYTES of heap payload (headers have empty strings).
// Terminal replies use a separate single slot: their existing provider response bound
// is independent of transient streaming delivery, and cancellation must not lose them.
const _: () = assert!(FRAGMENT_BYTES >= 4);

enum Frame {
    Header(ChatEvent),
    Field(usize, String),
    End,
}

pub(super) struct Sender {
    payload: SyncSender<Frame>,
    terminal: SyncSender<Box<crate::agent::Result<crate::llm::Reply>>>,
}

pub(super) struct Delivery {
    payload: Option<Receiver<Frame>>,
    terminal: Receiver<Box<crate::agent::Result<crate::llm::Reply>>>,
    partial: Option<ChatEvent>,
    partial_bytes: usize,
    max_event_bytes: usize,
}

pub(super) fn channel() -> (Sender, Delivery) {
    channel_with_limit(crate::llm::MAX_STREAM_BYTES)
}

fn channel_with_limit(max_event_bytes: usize) -> (Sender, Delivery) {
    let (payload, receiver) = mpsc::sync_channel(QUEUED_FRAGMENTS);
    let (terminal, result) = mpsc::sync_channel(1);
    (
        Sender { payload, terminal },
        Delivery {
            payload: Some(receiver),
            terminal: result,
            partial: None,
            partial_bytes: 0,
            max_event_bytes,
        },
    )
}

fn fields(event: &mut ChatEvent) -> Vec<&mut String> {
    match event {
        ChatEvent::HiddenText(text)
        | ChatEvent::Text(text)
        | ChatEvent::Reasoning(text)
        | ChatEvent::Notice(text) => vec![text],
        ChatEvent::ToolArguments {
            id, name, delta, ..
        } => vec![id, name, delta],
        ChatEvent::ToolCallComplete {
            id,
            name,
            arguments,
            ..
        } => vec![id, name, arguments],
        _ => vec![],
    }
}

impl Sender {
    pub(super) fn send(&self, mut event: ChatEvent) -> Result<(), ()> {
        if let ChatEvent::Finished(result) = event {
            return self.terminal.send(result).map_err(|_| ());
        }
        let values: Vec<String> = fields(&mut event).into_iter().map(std::mem::take).collect();
        self.payload.send(Frame::Header(event)).map_err(|_| ())?;
        for (index, value) in values.into_iter().enumerate() {
            let mut rest = value.as_str();
            while !rest.is_empty() {
                let mut end = rest.len().min(FRAGMENT_BYTES);
                while !rest.is_char_boundary(end) {
                    end -= 1;
                }
                self.payload
                    .send(Frame::Field(index, rest[..end].to_owned()))
                    .map_err(|_| ())?;
                rest = &rest[end..];
            }
        }
        self.payload.send(Frame::End).map_err(|_| ())
    }
}

impl Delivery {
    /// Drop the receiver before joining the provider: even a full queue's sender wakes.
    /// Keep the independent terminal slot, including provider errors and usage.
    pub(super) fn cancel(&mut self) {
        self.payload = None;
        self.partial = None;
        self.partial_bytes = 0;
    }

    pub(super) fn recv_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<ChatEvent, RecvTimeoutError> {
        let deadline = Instant::now() + timeout;
        loop {
            let Some(payload) = &self.payload else {
                return self
                    .terminal
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .map(ChatEvent::Finished);
            };
            match payload.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Frame::Header(event)) => {
                    self.partial = Some(event);
                    self.partial_bytes = 0;
                }
                Ok(Frame::Field(index, text)) => {
                    if text.len() > self.max_event_bytes.saturating_sub(self.partial_bytes) {
                        let limit = self.max_event_bytes;
                        self.cancel();
                        return Ok(ChatEvent::Finished(Box::new(Err(anyhow::anyhow!(
                            "model streaming event exceeds {limit} bytes"
                        )))));
                    }
                    self.partial_bytes += text.len();
                    if let Some(event) = &mut self.partial {
                        fields(event)[index].push_str(&text);
                    }
                }
                Ok(Frame::End) => {
                    if let Some(event) = self.partial.take() {
                        return Ok(event);
                    }
                }
                Err(RecvTimeoutError::Disconnected) => self.cancel(),
                Err(error) => return Err(error),
            }
            // Large events must not monopolize the controller's input/paint loop.
            if Instant::now() >= deadline {
                return Err(RecvTimeoutError::Timeout);
            }
        }
    }
}

#[cfg(test)]
mod tests;
