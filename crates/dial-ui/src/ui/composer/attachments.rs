//! Coalesced attachment preflight. Only the injected adapter reads workspace content.
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

use super::{Composer, is_slash_command};

pub type CandidateCheck = Arc<dyn Fn(&str) -> bool + Send + Sync>;
pub type PreviewProvider = Arc<dyn Fn(&str, &AtomicBool) -> Preview + Send + Sync>;
pub type ReferenceRemover = Arc<dyn Fn(&str, &str) -> Option<String> + Send + Sync>;

#[derive(Clone, Debug, Default)]
pub struct Preview {
    pub summary: String,
    pub issues: Vec<String>,
    pub items: Vec<PreviewItem>,
}

#[derive(Clone, Debug)]
pub struct PreviewItem {
    pub reference: String,
    pub detail: String,
}

pub struct Provider {
    pub candidates: CandidateCheck,
    pub preview: PreviewProvider,
    pub remove: ReferenceRemover,
}

struct Running {
    generation: u64,
    rx: mpsc::Receiver<Preview>,
    handle: std::thread::JoinHandle<()>,
}

#[derive(Default)]
pub(super) struct Preflight {
    provider: Option<Provider>,
    text: String,
    generation: u64,
    pending: Option<Instant>,
    running: Option<Running>,
    cancel: Arc<AtomicBool>,
    result: Option<Preview>,
    armed: bool,
}

impl Drop for Preflight {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(job) = self.running.take() {
            // Cancellation is cooperative. Never wait for workspace I/O on the UI thread;
            // a shutdown reaper owns and collects even a provider that later panics.
            if let Err(error) = std::thread::Builder::new()
                .name("attachment-preview-reap".into())
                .spawn(move || {
                    if job.handle.join().is_err() {
                        eprintln!("Attachment preview worker failed during shutdown");
                    }
                })
            {
                eprintln!("Could not start attachment preview shutdown reaper: {error}");
            }
        }
    }
}

impl Preflight {
    fn update(&mut self, text: String) {
        if self.text == text {
            return;
        }
        self.cancel.store(true, Ordering::Relaxed);
        self.generation = self.generation.wrapping_add(1);
        self.text = text;
        self.result = None;
        self.armed = false;
        self.pending = self
            .provider
            .as_ref()
            .filter(|p| {
                !is_slash_command(&self.text)
                    && !self.text.trim_start().starts_with('!')
                    && (p.candidates)(&self.text)
            })
            .map(|_| Instant::now());
    }

    fn failed(&mut self, generation: u64, reason: &str) -> bool {
        if generation != self.generation {
            return false;
        }
        self.result = Some(Preview {
            summary: "Attachments: preview unavailable · no inclusion verified".into(),
            issues: vec![format!(
                "Attachment preview failed: {reason}. Inclusion was not verified; inspect references or explicitly confirm sending without preview."
            )],
            items: Vec::new(),
        });
        self.armed = false;
        true
    }

    fn poll(&mut self) -> bool {
        let mut changed = false;
        // Joining only finished threads keeps input nonblocking, including after cancellation.
        // The worker alone owns the sender, so panic/drop cannot leave an immortal channel.
        if self
            .running
            .as_ref()
            .is_some_and(|job| job.handle.is_finished())
            && let Some(job) = self.running.take()
        {
            let joined = job.handle.join();
            match (joined, job.rx.try_recv()) {
                (Ok(()), Ok(result)) if job.generation == self.generation => {
                    self.result = Some(result);
                    changed = true;
                }
                (Ok(()), Ok(_)) => {}
                (Err(_), _) => changed |= self.failed(job.generation, "worker panicked"),
                (Ok(()), Err(_)) => {
                    changed |= self.failed(job.generation, "worker exited without a result")
                }
            }
        }
        if self.running.is_none()
            && self
                .pending
                .is_some_and(|at| at.elapsed() >= Duration::from_millis(150))
        {
            self.pending = None;
            if let Some(provider) = &self.provider {
                let preview = Arc::clone(&provider.preview);
                let text = self.text.clone();
                let generation = self.generation;
                let (tx, rx) = mpsc::channel();
                self.cancel = Arc::default();
                let cancel = Arc::clone(&self.cancel);
                match std::thread::Builder::new()
                    .name("attachment-preview".into())
                    .spawn(move || {
                        let result = preview(&text, &cancel);
                        let _ = tx.send(result);
                    }) {
                    Ok(handle) => {
                        self.running = Some(Running {
                            generation,
                            rx,
                            handle,
                        })
                    }
                    Err(_) => changed |= self.failed(generation, "could not start worker"),
                }
            }
        }
        changed
    }

    pub(super) fn pending(&self) -> bool {
        self.pending.is_some() || self.running.is_some()
    }

    pub(super) fn summary(&self) -> Option<String> {
        if self.pending() {
            return Some("Attachments: checking… · draft kept until ready".into());
        }
        self.result.as_ref().map(|result| {
            if self.armed {
                "Attachments need attention · submit again to send anyway · /attachments inspect"
                    .into()
            } else {
                format!("{} · /attachments inspect", result.summary)
            }
        })
    }
}

impl Composer {
    pub fn set_attachment_provider(&mut self, provider: Option<Provider>) {
        self.attachments.cancel.store(true, Ordering::Relaxed);
        self.attachments.provider = provider;
        // Invalidate even if the new workspace has the identical draft.
        self.attachments.text.clear();
        self.attachments.generation = self.attachments.generation.wrapping_add(1);
        self.attachments.result = None;
        self.attachments.armed = false;
        self.attachments.pending = None;
        self.refresh_attachments();
    }

    pub(super) fn refresh_attachments(&mut self) {
        self.attachments.update(self.expanded_draft());
    }

    pub(super) fn poll_attachments(&mut self) -> bool {
        self.refresh_attachments();
        self.attachments.poll()
    }

    pub fn attachment_items(&mut self) -> Vec<PreviewItem> {
        self.poll_attachments();
        if self.attachments.pending() {
            return Vec::new();
        }
        self.attachments
            .result
            .as_ref()
            .map_or_else(Vec::new, |p| p.items.clone())
    }

    pub(super) fn attachment_send_allowed(&mut self) -> bool {
        self.poll_attachments();
        if self.attachments.pending() {
            self.queue_notice =
                Some("Attachments are still being checked. Draft kept; submit when ready.".into());
            return false;
        }
        if self
            .attachments
            .result
            .as_ref()
            .is_some_and(|p| !p.issues.is_empty())
            && !self.attachments.armed
        {
            self.attachments.armed = true;
            self.queue_notice = Some("Attachment content is missing or truncated. Submit again to send anyway, or /attachments inspect.".into());
            return false;
        }
        true
    }

    /// Inspection is cached metadata, never a read on the rendering/input thread.
    pub fn attachment_report(&mut self) -> String {
        self.poll_attachments();
        if self.attachments.pending() {
            return "Attachment preview pending. Draft kept; inspect again when ready.".into();
        }
        let Some(result) = &self.attachments.result else {
            return "No attachments in the current draft.".into();
        };
        let mut rows = vec![result.summary.clone()];
        rows.extend(
            result
                .items
                .iter()
                .enumerate()
                .map(|(i, item)| format!("{}: {} — {}", i + 1, item.reference, item.detail)),
        );
        rows.extend(
            result
                .issues
                .iter()
                .map(|issue| format!("Warning: {issue}")),
        );
        rows.push("/attachments remove <number> removes a reference from the draft. Nothing is sent by inspection.".into());
        rows.join("\n")
    }

    pub fn remove_attachment(&mut self, index: usize) -> bool {
        self.poll_attachments();
        if self.attachments.pending() {
            return false;
        }
        let Some(item) = self
            .attachments
            .result
            .as_ref()
            .and_then(|p| p.items.get(index))
        else {
            return false;
        };
        let Some(provider) = &self.attachments.provider else {
            return false;
        };
        let Some(text) = (provider.remove)(&self.expanded_draft(), &item.reference) else {
            return false;
        };
        self.set_draft(&text);
        true
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod key_tests;
