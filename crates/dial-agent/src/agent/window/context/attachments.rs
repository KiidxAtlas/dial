//! Attachment facts produced by the same bounded expansion that prepares a user message.

/// What an explicitly referenced item contributes to a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentKind {
    File,
    Directory,
    Image,
    Changes,
}

/// Whether the referenced content could be included within the expansion limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentState {
    Included,
    Truncated,
    Skipped,
}

/// Bounded metadata, never a second copy of an attachment's contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    pub reference: String,
    pub kind: AttachmentKind,
    pub state: AttachmentState,
    /// Retained text bytes, including the request's wrapper; images are counted separately.
    pub text_bytes: usize,
    pub detail: String,
}

impl Attachment {
    pub(super) fn included(reference: &str, kind: AttachmentKind, text_bytes: usize) -> Self {
        Self {
            reference: reference.to_owned(),
            kind,
            state: AttachmentState::Included,
            text_bytes,
            detail: "included".into(),
        }
    }

    pub(super) fn skipped(
        reference: &str,
        kind: AttachmentKind,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            reference: reference.to_owned(),
            kind,
            state: AttachmentState::Skipped,
            text_bytes: 0,
            detail: detail.into(),
        }
    }
}

/// A prepared message and the attachment evidence observed while constructing it.
#[derive(Clone, Debug)]
pub struct PreparedMessage {
    pub message: serde_json::Value,
    pub warnings: Vec<String>,
    pub attachments: Vec<Attachment>,
}

impl PreparedMessage {
    /// Only missing, clipped or skipped input warrants an extra send decision.
    pub fn issues(&self) -> Vec<String> {
        let mut issues: Vec<_> = self
            .attachments
            .iter()
            .filter(|item| item.state != AttachmentState::Included)
            .map(|item| item.detail.clone())
            .collect();
        for warning in &self.warnings {
            if !issues.iter().any(|issue| issue == warning) {
                issues.push(warning.clone());
            }
        }
        issues
    }

    /// A text-only estimate; image token costs are deliberately not invented.
    pub fn summary(&self) -> String {
        let included: Vec<_> = self
            .attachments
            .iter()
            .filter(|item| item.state != AttachmentState::Skipped)
            .collect();
        let images = included
            .iter()
            .filter(|item| item.kind == AttachmentKind::Image)
            .count();
        let text = included.len().saturating_sub(images);
        let bytes: usize = included.iter().map(|item| item.text_bytes).sum();
        let mut parts = Vec::new();
        if text > 0 {
            parts.push(format!(
                "{text} text attachment(s) · ~{} text tokens",
                bytes.div_ceil(4)
            ));
        }
        if images > 0 {
            parts.push(format!("{images} image(s) · image tokens vary"));
        }
        let issues = self.issues().len();
        if issues > 0 {
            parts.push(format!("{issues} attachment issue(s)"));
        }
        parts.join(" · ")
    }
}

pub(super) struct PreparedReferences {
    pub content: String,
    pub warnings: Vec<String>,
    pub attachments: Vec<Attachment>,
}

/// Apply only spans admitted by the canonical expansion parsers. Keeping surrounding text
/// byte-for-byte avoids editing prose, email addresses or similarly named paths by accident.
pub(super) fn remove_reference(text: &str, reference: &str) -> Option<String> {
    let mut spans: Vec<_> = super::reference_spans(text)
        .into_iter()
        .chain(super::pasted_image_spans(text))
        .filter(|item| item.token == reference)
        .map(|item| item.span)
        .collect();
    if spans.is_empty() {
        return None;
    }
    spans.sort_by_key(|span| (span.start, span.end));
    let mut result = String::with_capacity(text.len());
    let mut cursor = 0;
    for span in spans {
        if span.start >= cursor {
            result.push_str(&text[cursor..span.start]);
        }
        cursor = cursor.max(span.end);
    }
    result.push_str(&text[cursor..]);
    Some(result)
}

#[cfg(test)]
mod tests;
