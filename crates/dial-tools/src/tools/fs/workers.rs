//! Invocation-local workers for bounded, ordered filesystem preparation. Callers decide which
//! jobs may be speculated; only their coordinator accounts for budgets or publishes observations.

use std::sync::mpsc;

pub(super) type OrderedMap<'a, I, O> = dyn FnMut(&[I]) -> Vec<O> + 'a;

/// Start threads only when the caller submits a parallel batch. Each lane accepts one item at a
/// time; receiving the wave in dispatch order keeps completion timing out of the result. Dropping
/// the channels stops idle workers and the scope joins them before the tool returns.
pub(super) fn with_workers<I, O, R>(
    width: usize,
    work: impl Fn(I) -> O + Clone + Send,
    run: impl FnOnce(&mut OrderedMap<'_, I, O>) -> R,
) -> R
where
    I: Clone + Send,
    O: Send,
{
    std::thread::scope(|scope| {
        let mut lanes = Vec::new();
        let mut limit = width;
        run(&mut |items| {
            if limit < 2 || items.len() < 2 {
                return items.iter().cloned().map(&work).collect();
            }
            while lanes.len() < limit.min(items.len()) {
                let (submit, jobs) = mpsc::sync_channel(1);
                let (done, results) = mpsc::sync_channel(1);
                let work = work.clone();
                let worker = std::thread::Builder::new().spawn_scoped(scope, move || {
                    for job in jobs {
                        if done.send(work(job)).is_err() {
                            break;
                        }
                    }
                });
                if worker.is_err() {
                    limit = lanes.len();
                    break;
                }
                lanes.push((submit, results));
            }
            if lanes.is_empty() {
                return items.iter().cloned().map(&work).collect();
            }
            let mut out = Vec::with_capacity(items.len());
            for wave in items.chunks(lanes.len()) {
                for (item, (submit, _)) in wave.iter().zip(&lanes) {
                    submit
                        .send(item.clone())
                        .expect("filesystem worker stopped");
                }
                for (_, results) in &lanes[..wave.len()] {
                    out.push(results.recv().expect("filesystem worker stopped"));
                }
            }
            out
        })
    })
}

#[cfg(test)]
mod tests;
