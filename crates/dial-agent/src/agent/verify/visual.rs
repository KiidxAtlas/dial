//! Bounded prompts for useful visual evidence; pixels are observations, not proof of correctness.
use std::{collections::HashSet, path::Path};

pub type Revision = super::evidence::Revision;

/// The prompt with compound identifiers (`dial-ui`, `src/ui/mod.rs`, `ui::state`) replaced by a
/// neutral placeholder, so their parts do not count as standalone words. Line structure and
/// indentation are kept for the prose filter.
fn without_compound_identifiers(prompt: &str) -> String {
    prompt
        .split_inclusive(char::is_whitespace)
        .map(|piece| {
            let token = piece.trim_end();
            let core = token.trim_matches(|c: char| !(c.is_alphanumeric() || c == '_'));
            if core.contains(['-', '/', '.', ':']) {
                format!("_{}", &piece[token.len()..])
            } else {
                piece.to_owned()
            }
        })
        .collect()
}

pub fn useful(prompt: &str) -> bool {
    let words: HashSet<String> = dial_core::prose::words(prompt).collect();
    let any = |terms: &[&str]| terms.iter().any(|term| words.contains(*term));
    // UI subjects and edit verbs count only as standalone words: crate names and paths such as
    // `dial-ui` or `ui/fix.rs` are not requests for visual work.
    let plain = without_compound_identifiers(prompt);
    let standalone: HashSet<String> = dial_core::prose::words(&plain).collect();
    let any_standalone = |terms: &[&str]| terms.iter().any(|term| standalone.contains(*term));
    // Words that describe how something looks, not merely that it changes.
    const APPEARANCE: &[&str] = &[
        "design",
        "look",
        "looks",
        "overlap",
        "spacing",
        "color",
        "alignment",
        "render",
        "layout",
    ];
    any(&["screenshot", "visually", "appearance"])
        || (words.contains("visual") && any(&["inspect", "inspection", "verify", "verification"]))
        || (any_standalone(&[
            "website", "webpage", "frontend", "ui", "css", "layout", "button", "image", "picture",
            "photo",
        ]) && (any_standalone(&["fix", "edit", "change", "build", "create", "inspect", "compare"])
            || any(APPEARANCE))
            && !any(&["docker", "container"]))
        // "page" is also pagination ("page size", `page_count`), so it needs an appearance word.
        || (words.contains("page") && any(APPEARANCE))
}

fn rendered_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|p| p.to_str()).unwrap_or("");
    if name.contains(".test.") || name.contains(".spec.") {
        return false;
    }
    matches!(
        path.extension().and_then(|p| p.to_str()),
        Some("html" | "htm" | "css" | "scss" | "sass" | "less" | "jsx" | "tsx" | "vue" | "svelte")
    )
}
pub use crate::agent::prompts::recovery::INITIAL_GUIDANCE;

#[derive(Default)]
pub struct Review {
    required: bool,
    render_required: bool,
    rendered_pixels: Vec<(Revision, Option<String>)>,
    image_pixels: Option<Revision>,
    pending_rendered: Vec<(Revision, Option<String>)>,
    pending_image: Option<Revision>,
}

impl Review {
    pub fn new(prompt: &str) -> Self {
        Self {
            required: useful(prompt),
            ..Self::default()
        }
    }

    pub fn require_rendered(&mut self) {
        self.required = true;
        self.render_required = true;
    }

    pub fn rendered_observed(&self, revision: Revision) -> bool {
        self.rendered_pixels
            .iter()
            .any(|(seen, _)| *seen == revision)
    }

    pub fn rendered_target_observed(&self, revision: Revision, target: &str) -> bool {
        !target.is_empty()
            && self
                .rendered_pixels
                .iter()
                .any(|(seen, locator)| *seen == revision && locator.as_deref() == Some(target))
    }

    pub fn changed(&mut self, path: &Path) {
        if rendered_file(path) {
            self.required = true;
            self.render_required = true;
        }
    }

    /// User-supplied/source images never establish rendered-target evidence.
    pub fn user_image(&mut self, revision: Revision) {
        self.pending_image = Some(revision);
    }

    pub fn capture_output(
        &mut self,
        output: &crate::runtime::tool_output::ToolOutput,
        revision: Revision,
        images: &serde_json::Value,
    ) {
        if output.is_error {
            return;
        }
        for image in images.as_array().into_iter().flatten() {
            let Ok((bytes, media_type)) = crate::llm::images::verified_bytes(image) else {
                continue;
            };
            self.pending_image = Some(revision);
            for capture in &output.execution.captures {
                if capture.matches(&bytes, media_type) {
                    let observed = (revision, capture.target.clone());
                    if !self.pending_rendered.contains(&observed) {
                        self.pending_rendered.push(observed);
                    }
                }
            }
        }
    }

    #[cfg(test)]
    pub fn captured(&mut self, revision: Revision, rendered: bool) {
        self.user_image(revision);
        if rendered {
            self.pending_rendered.push((revision, None));
        }
    }

    /// Count a capture only after a subsequent model request succeeds, not in the same tool batch.
    pub fn delivered(&mut self) {
        if let Some(revision) = self.pending_image.take() {
            self.image_pixels = Some(revision);
        }
        if !self.pending_rendered.is_empty() {
            let latest = self.pending_rendered.last().unwrap().0;
            self.rendered_pixels
                .retain(|(revision, _)| *revision == latest);
            for observed in self
                .pending_rendered
                .drain(..)
                .filter(|(revision, _)| *revision == latest)
            {
                if !self.rendered_pixels.contains(&observed) {
                    self.rendered_pixels.push(observed);
                }
            }
        }
    }

    fn missing(&self, revision: Revision) -> bool {
        self.required
            && if self.render_required {
                !self.rendered_observed(revision)
            } else {
                self.image_pixels != Some(revision)
            }
    }

    /// Evidence is delivered pixels for the latest effect revision, not a correctness verdict.
    pub fn status(&self, revision: Revision) -> &'static str {
        if !self.required {
            "not_required"
        } else if self.missing(revision) {
            "missing"
        } else {
            "observed"
        }
    }

    pub fn summary(&self, text: String, revision: Revision) -> String {
        if self.missing(revision) {
            format!("{text}\n\nVisual inspection was not completed for the latest result.")
        } else {
            text
        }
    }
}

#[cfg(test)]
mod tests;
