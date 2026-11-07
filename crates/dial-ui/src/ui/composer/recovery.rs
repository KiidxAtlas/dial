//! Bounded, inert composer snapshots. Persistence belongs to the application, not this view.
use super::{Composer, QueuedEdit, SteerKind, SteerStage};
use anyhow::{Result, ensure};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const TEXT_LIMIT: usize = 2 * 1024 * 1024;
const IMAGE_LIMIT: usize = crate::llm::images::MAX_IMAGE_BYTES as usize;

/// Called synchronously before input ownership passes to an execution consumer.
pub type RecoverySink = Arc<dyn Fn(&RecoverySnapshot) -> Result<()> + Send + Sync>;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecoverySnapshot {
    draft: String,
    pastes: Vec<(String, String)>,
    paste_count: usize,
    queued: Vec<(String, bool)>,
    explicit: Vec<(String, bool)>,
    editing: Option<(usize, String, bool)>,
    images: Vec<(String, String)>,
}

impl RecoverySnapshot {
    /// Read-only recovery review; never exposes embedded clipboard image payloads.
    /// Long content is explicitly abbreviated to keep the terminal panel bounded.
    pub fn preview_text(&self) -> String {
        fn excerpt(text: &str) -> String {
            let mut chars = text.chars();
            let mut shown: String = chars.by_ref().take(4000).collect();
            if chars.next().is_some() {
                shown.push_str("\n[preview abbreviated; full text remains saved]");
            }
            shown
        }
        let expand = |text: &str| {
            let mut shown = excerpt(text);
            for (marker, content) in &self.pastes {
                let replacement = excerpt(content);
                let expanded: String = shown
                    .split(marker)
                    .enumerate()
                    .flat_map(|(index, part)| {
                        [if index == 0 { "" } else { replacement.as_str() }, part]
                    })
                    .flat_map(str::chars)
                    .take(4001)
                    .collect();
                shown = excerpt(&expanded);
            }
            shown
        };
        let mut lines = vec![format!(
            "Draft:\n{}\n\nClipboard images: {} (payloads not displayed)",
            if self.draft.is_empty() {
                "[empty]".into()
            } else {
                expand(&self.draft)
            },
            self.images.len()
        )];
        for (index, (text, follow_up)) in self.queued.iter().enumerate() {
            lines.push(format!(
                "Queue {} · {}:\n{}",
                index + 1,
                if *follow_up {
                    "follow-up"
                } else {
                    "automatic steering"
                },
                expand(text)
            ));
        }
        for (index, (text, independent)) in self.explicit.iter().enumerate() {
            lines.push(format!(
                "Request {} · {}:\n{}",
                index + 1,
                if *independent {
                    "independent"
                } else {
                    "steer now"
                },
                expand(text)
            ));
        }
        if let Some((index, text, follow_up)) = &self.editing {
            lines.push(format!(
                "Editing queue {} · {}:\n{}",
                index + 1,
                if *follow_up {
                    "follow-up"
                } else {
                    "automatic steering"
                },
                expand(text)
            ));
        }
        lines.join("\n\n")
    }

    pub fn is_empty(&self) -> bool {
        self.draft.is_empty()
            && self.queued.is_empty()
            && self.explicit.is_empty()
            && self.editing.is_none()
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.paste_count <= u32::MAX as usize,
            "invalid recovered paste counter"
        );
        ensure!(
            self.queued.len() <= 32 && self.explicit.len() <= 16,
            "recovered queue exceeds limits"
        );
        ensure!(
            self.pastes.len() <= 256 && self.images.len() <= 32,
            "too many recovered attachments"
        );
        ensure!(
            self.pastes.iter().all(|(marker, _)| !marker.is_empty()),
            "invalid recovered paste marker"
        );
        ensure!(
            self.images
                .iter()
                .all(|(path, _)| !path.is_empty() && path.len() <= 4096),
            "invalid recovered image path"
        );
        let bytes = self.draft.len()
            + self
                .pastes
                .iter()
                .map(|(a, b)| a.len() + b.len())
                .sum::<usize>()
            + self.queued.iter().map(|(t, _)| t.len()).sum::<usize>()
            + self.explicit.iter().map(|(t, _)| t.len()).sum::<usize>()
            + self.editing.as_ref().map_or(0, |(_, t, _)| t.len());
        ensure!(bytes <= TEXT_LIMIT, "recovered text exceeds 2 MiB");
        ensure!(
            self.images
                .iter()
                .all(|(_, payload)| payload.len() <= IMAGE_LIMIT * 4 / 3 + 4),
            "a recovered clipboard image exceeds the canonical image limit"
        );
        Ok(())
    }
    /// Storage may replace embedded image data with authenticated private sidecar references.
    pub fn map_image_payloads(
        &mut self,
        mut map: impl FnMut(&str) -> Result<String>,
    ) -> Result<()> {
        for (_, payload) in &mut self.images {
            *payload = map(payload)?;
        }
        Ok(())
    }

    fn replace_path(&mut self, old: &str, new: &str) {
        self.draft = self.draft.replace(old, new);
        for (_, text) in &mut self.pastes {
            *text = text.replace(old, new);
        }
        for (text, _) in self.queued.iter_mut().chain(&mut self.explicit) {
            *text = text.replace(old, new);
        }
        if let Some((_, text, _)) = &mut self.editing {
            *text = text.replace(old, new);
        }
    }
}

pub(super) enum Take {
    Draft,
    FollowUp,
    Steering,
    Explicit(SteerKind),
}

impl Composer {
    pub fn set_recovery_sink(&mut self, sink: Option<RecoverySink>) {
        self.recovery_sink = sink;
    }

    /// Only unreceived requests are retained. Received/applied requests are never replay candidates.
    pub fn recovery_snapshot(&self) -> Result<RecoverySnapshot> {
        let mut snapshot = RecoverySnapshot {
            draft: self.editor.text.clone(),
            pastes: self.pastes.clone(),
            paste_count: self.paste_count,
            queued: self.queued.iter().cloned().collect(),
            explicit: self
                .explicit
                .iter()
                .filter(|item| item.untaken && item.stage == SteerStage::Pending)
                .map(|item| (item.text.clone(), item.kind == SteerKind::Independent))
                .collect(),
            editing: self
                .editing_queue
                .as_ref()
                .map(|item| (item.index, item.text.clone(), item.follow_up)),
            images: Vec::new(),
        };
        let mut image_bytes = 0usize;
        for path in &self.pasted_images {
            let name = path.to_string_lossy().into_owned();
            let referenced = snapshot.draft.contains(&name)
                || snapshot.pastes.iter().any(|(_, t)| t.contains(&name))
                || snapshot
                    .queued
                    .iter()
                    .chain(&snapshot.explicit)
                    .any(|(t, _)| t.contains(&name))
                || snapshot
                    .editing
                    .as_ref()
                    .is_some_and(|(_, t, _)| t.contains(&name));
            if !referenced {
                continue;
            }
            // Clipboard files are ours, not arbitrary @mentions or project attachments.
            let file = crate::platform::fs::open_nofollow_nonblocking(
                std::fs::OpenOptions::new().read(true),
                path,
            )?;
            crate::platform::atomic::check_file(&file)?;
            let size = usize::try_from(file.metadata()?.len())?;
            image_bytes = image_bytes.saturating_add(size);
            ensure!(
                size <= IMAGE_LIMIT && image_bytes <= IMAGE_LIMIT * 32,
                "clipboard recovery exceeds the canonical per-image limit or 32-image recovery bound"
            );
            let mut bytes = Vec::with_capacity(size);
            std::io::Read::read_to_end(
                &mut std::io::Read::take(file, IMAGE_LIMIT as u64 + 1),
                &mut bytes,
            )?;
            ensure!(
                bytes.len() == size,
                "clipboard attachment changed while recovering"
            );
            snapshot.images.push((
                name,
                base64::engine::general_purpose::STANDARD.encode(bytes),
            ));
        }
        snapshot.validate()?;
        Ok(snapshot)
    }

    /// Restore is inert and refuses to replace input unless the caller explicitly chose replacement.
    /// Image payloads are materialized privately before any live composer state changes.
    pub fn restore_recovery(
        &mut self,
        mut snapshot: RecoverySnapshot,
        replace: bool,
    ) -> Result<()> {
        snapshot.validate()?;
        ensure!(
            replace || self.recovery_snapshot()?.is_empty(),
            "current input is not empty; choose replace explicitly"
        );
        let mut images = Vec::new();
        let mut bytes_total = 0usize;
        for (old, encoded) in snapshot.images.clone() {
            let bytes = base64::engine::general_purpose::STANDARD.decode(encoded)?;
            bytes_total = bytes_total.saturating_add(bytes.len());
            ensure!(
                !bytes.is_empty() && bytes.len() <= IMAGE_LIMIT && bytes_total <= IMAGE_LIMIT * 32,
                "invalid recovered clipboard image"
            );
            let mut file = tempfile::Builder::new()
                .prefix("dial-paste-")
                .suffix(".png")
                .tempfile()?;
            crate::platform::fs::set_owner_only(file.path())?;
            std::io::Write::write_all(&mut file, &bytes)?;
            let path = file.into_temp_path();
            snapshot.replace_path(&old, &path.to_string_lossy());
            images.push(path);
        }
        snapshot.validate()?;
        self.clear_recovery_input();
        self.pasted_images.extend(images);
        self.editor.set(&snapshot.draft);
        self.pastes = snapshot.pastes;
        self.paste_count = snapshot.paste_count;
        self.queued = snapshot.queued.into();
        self.explicit = snapshot
            .explicit
            .into_iter()
            .map(|(text, independent)| super::Explicit {
                text,
                kind: if independent {
                    SteerKind::Independent
                } else {
                    SteerKind::Steer
                },
                stage: SteerStage::Pending,
                untaken: true,
            })
            .collect();
        self.editing_queue = snapshot.editing.map(|(index, text, follow_up)| QueuedEdit {
            index,
            text,
            follow_up,
        });
        self.recovery_held = true;
        self.queue_notice = Some("Recovered input held. Enter sends a draft/oldest queue item; /draft resume releases queued intent.".into());
        self.refresh();
        Ok(())
    }

    /// Session transitions must not transfer one session's editor or queue into another.
    pub fn clear_recovery_input(&mut self) {
        self.editing_queue = None;
        self.queued.clear();
        self.explicit.clear();
        self.recovery_held = false;
        self.clipboard_read = None;
        self.pastes.clear();
        self.paste_count = 0;
        self.clear();
        // Keep already-submitted image files alive until the shell exits.
    }

    pub fn resume_recovered_queue(&mut self) {
        self.recovery_held = false;
    }

    pub(super) fn recovery_before_take(&mut self, take: Take) -> bool {
        let Some(sink) = self.recovery_sink.clone() else {
            return true;
        };
        if matches!(take, Take::Draft)
            && self.editor.text.split_whitespace().next() == Some("/draft")
        {
            // Recovery controls remain usable even when a corrupt record requires explicit discard.
            return true;
        }
        let result = (|| {
            let mut snapshot = self.recovery_snapshot()?;
            match take {
                Take::Draft => {
                    snapshot.draft.clear();
                    snapshot.pastes.clear();
                    snapshot.editing = None;
                }
                Take::FollowUp => {
                    if !snapshot.queued.is_empty() {
                        snapshot.queued.remove(0);
                    }
                }
                Take::Steering => snapshot.queued.retain(|(_, follow_up)| *follow_up),
                Take::Explicit(kind) => snapshot
                    .explicit
                    .retain(|(_, independent)| *independent != (kind == SteerKind::Independent)),
            }
            sink(&snapshot)
        })();
        if let Err(error) = result {
            self.queue_notice = Some(format!("Input recovery save failed; not sent: {error:#}"));
            false
        } else {
            true
        }
    }
}

#[cfg(test)]
mod tests;
