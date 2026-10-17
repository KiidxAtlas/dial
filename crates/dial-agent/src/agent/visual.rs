//! Bounded prompts for useful visual evidence; pixels are observations, not proof of correctness.
use std::{collections::HashSet, path::Path};

pub type Revision = (u64, u64);

pub fn useful(prompt: &str) -> bool {
    let lower = prompt.to_lowercase();
    let words: HashSet<_> = lower.split(|c: char| !c.is_alphanumeric()).collect();
    let any = |terms: &[&str]| terms.iter().any(|term| words.contains(term));
    any(&["screenshot", "visually", "appearance"])
        || (any(&[
            "website", "webpage", "page", "frontend", "ui", "css", "layout", "button", "image",
            "picture", "photo",
        ]) && any(&[
            "fix",
            "edit",
            "change",
            "build",
            "create",
            "design",
            "inspect",
            "look",
            "compare",
            "looks",
            "overlap",
            "spacing",
            "color",
            "alignment",
            "render",
        ]) && !any(&["docker", "container"]))
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

pub const INITIAL_GUIDANCE: &str = "Visual evidence may help this task. Decide what needs to be seen and inspect it proactively, without waiting for the user to ask for screenshots. For a site, discover/load browser, open the actual relevant page or approved local preview, and capture a baseline before visual edits when available. Inspect supplied images with read when needed. After edits, capture and inspect the updated result. DOM/source text and a passing build do not show appearance. Preserve browser/network approvals and report unavailable rendering or image input honestly.";

#[derive(Default)]
pub struct Review {
    required: bool,
    render_required: bool,
    browser_pixels: Option<Revision>,
    image_pixels: Option<Revision>,
    pending_browser: Option<Revision>,
    pending_image: Option<Revision>,
}

impl Review {
    pub fn new(prompt: &str) -> Self {
        Self {
            required: useful(prompt),
            ..Self::default()
        }
    }

    pub fn changed(&mut self, path: &Path) {
        if rendered_file(path) {
            self.required = true;
            self.render_required = true;
        }
    }

    pub fn captured(&mut self, revision: Revision, browser: bool) {
        self.pending_image = Some(revision);
        if browser {
            self.pending_browser = Some(revision);
        }
    }

    /// Count a capture only after a subsequent model request succeeds, not in the same tool batch.
    pub fn delivered(&mut self) {
        if let Some(revision) = self.pending_image.take() {
            self.image_pixels = Some(revision);
        }
        if let Some(revision) = self.pending_browser.take() {
            self.browser_pixels = Some(revision);
        }
    }

    fn missing(&self, revision: Revision) -> bool {
        self.required
            && (if self.render_required {
                self.browser_pixels
            } else {
                self.image_pixels
            }) != Some(revision)
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
