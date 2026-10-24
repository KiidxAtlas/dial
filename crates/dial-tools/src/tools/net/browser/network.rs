//! Operation-owned HTTP jobs. Polling keeps the driver response pipe responsive while a
//! request waits for headers/body. Jobs never outlive the approved browser operation.
use super::*;
use std::collections::HashMap;
use std::sync::mpsc::{self, TryRecvError};

struct CancelOnDrop(Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

pub(super) fn exchange(
    process: &mut JsonProcess,
    request: Value,
    timeout: Duration,
    allowed_hosts: &[String],
    interrupt: &AtomicBool,
) -> Result<Value, String> {
    let end = execution_deadline(timeout);
    std::thread::scope(|scope| {
        let cancel = CancelOnDrop(Arc::new(AtomicBool::new(false)));
        let mut jobs = HashMap::new();
        let mut admitted = 0usize;
        let mut bytes = 0usize;
        let result = process.request_guarded(
            request,
            end,
            || {
                if interrupt.load(Ordering::Acquire) {
                    cancel.0.store(true, Ordering::Release);
                    Err("browser operation cancelled; effects may already have occurred".into())
                } else {
                    Ok(())
                }
            },
            usize::MAX,
            |event| match event["callback"].as_str() {
                Some("http") => {
                    let size = event.to_string().len();
                    if jobs.len() >= 16
                        || bytes.saturating_add(size) > 2 * 1024 * 1024
                        || admitted >= 1024
                    {
                        return Err("browser HTTP job budget exceeded".into());
                    }
                    let timeout = remaining_timeout(end)?;
                    let id = event["callback_id"]
                        .as_u64()
                        .ok_or("HTTP callback identity missing")?;
                    let event = event.clone();
                    let stopped = cancel.0.clone();
                    let (tx, rx) = mpsc::sync_channel(1);
                    scope.spawn(move || {
                        let _ = tx.send(bridge(&event, timeout, allowed_hosts, stopped));
                    });
                    jobs.insert(id, (rx, size));
                    admitted += 1;
                    bytes += size;
                    Ok(json!({"job":id}))
                }
                Some("http_poll") => {
                    let id = event["job"].as_u64().ok_or("HTTP job identity missing")?;
                    let (rx, size) = jobs
                        .get(&id)
                        .ok_or("HTTP job is not owned by this operation")?;
                    let outcome = match rx.try_recv() {
                        Ok(outcome) => outcome,
                        Err(TryRecvError::Empty) => return Ok(json!({"done":false})),
                        Err(TryRecvError::Disconnected) => {
                            Err("browser HTTP worker stopped without a result".into())
                        }
                    };
                    bytes -= size;
                    jobs.remove(&id);
                    Ok(match outcome {
                        Ok(value) => json!({"done":true,"value":value}),
                        Err(error) => json!({"done":true,"error":error}),
                    })
                }
                _ => Err("unsupported browser callback".into()),
            },
        );
        // Cancellation is published before scope joins, including on unwind. The guarded HTTP
        // transport polls it while waiting for headers/body; connect/resolve have phase bounds.
        drop(cancel);
        result
    })
}
