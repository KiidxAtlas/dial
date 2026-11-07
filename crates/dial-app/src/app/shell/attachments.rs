//! Application adapter for the composer's canonical attachment preflight.
use std::path::Path;
use std::sync::Arc;

use crate::agent::window::context;
use crate::ui::composer::{
    Composer,
    attachments::{Preview, PreviewItem, Provider},
};

use super::Shell;

pub(super) fn install(composer: &mut Composer, root: &Path) {
    let root = root.to_path_buf();
    composer.set_attachment_provider(Some(Provider {
        candidates: Arc::new(context::has_attachment_candidates),
        preview: Arc::new(move |text, cancel| {
            let prepared = context::prepare_message(&root, text, cancel);
            let mut counts = [0usize; 3];
            for item in &prepared.attachments {
                counts[match item.state {
                    context::AttachmentState::Included => 0,
                    context::AttachmentState::Truncated => 1,
                    context::AttachmentState::Skipped => 2,
                }] += 1;
            }
            Preview {
                summary: format!("Attachments: {} included · {} truncated · {} skipped · ~{} text tokens · {} image(s), image tokens vary", counts[0], counts[1], counts[2], prepared.attachments.iter().map(|item| item.text_bytes).sum::<usize>().div_ceil(4), prepared.attachments.iter().filter(|item| item.kind == context::AttachmentKind::Image && item.state != context::AttachmentState::Skipped).count()),
                issues: prepared.issues(),
                items: prepared.attachments.iter().map(|item| PreviewItem {
                    reference: item.reference.clone(),
                    detail: format!("{:?} · {:?} · {}", item.kind, item.state, item.detail),
                }).collect(),
            }
        }),
        remove: Arc::new(context::remove_attachment_reference),
    }));
}

impl Shell {
    /// Inspect/remove only; never dispatches a request or replaces an unrelated draft.
    pub(in crate::app) fn attachments_command(&mut self, args: &str) {
        let words: Vec<_> = args.split_whitespace().collect();
        let report = match words.as_slice() {
            [] => {
                let items = self.composer.attachment_items();
                if items.is_empty() {
                    self.composer.attachment_report()
                } else {
                    let labels: Vec<_> = items.iter().map(|item| (item.reference.as_str(), ' ')).collect();
                    if let Some(index) = self.ask("Attachments", "Choose an attachment to inspect. Nothing is sent or removed by selection.", &labels)
                        && let Some(item) = items.get(index)
                        && self.ask("Attachment", &format!("{}\n{}\nRemoval only edits this draft; it never deletes a file.", item.reference, item.detail), &[("Keep", 'k'), ("Remove reference", 'r')]) == Some(1)
                    {
                        if self.composer.remove_attachment(index) {
                            "Attachment reference removed; preview is refreshing.".into()
                        } else {
                            "Preview changed or is pending. Draft kept; inspect again.".into()
                        }
                    } else {
                        "Draft kept. No attachments sent.".into()
                    }
                }
            }
            ["inspect"] => self.composer.attachment_report(),
            ["remove", number] => match number.parse::<usize>().ok().and_then(|n| n.checked_sub(1)) {
                Some(index) if self.composer.remove_attachment(index) => "Attachment reference removed from the draft; preview is refreshing.".into(),
                _ => "Could not remove that attachment. Wait for preview, then /attachments inspect for current numbers. Draft kept.".into(),
            },
            _ => "Usage: /attachments [inspect|remove <number>]. Inspection never sends the draft.".into(),
        };
        self.note("Attachments", report);
    }
}
