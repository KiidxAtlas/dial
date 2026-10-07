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
    reminded: Option<Revision>,
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

    pub fn reminder(&mut self, revision: Revision) -> Option<&'static str> {
        if !self.missing(revision) || self.reminded == Some(revision) {
            return None;
        }
        self.reminded = Some(revision);
        Some(
            "Visual inspection checkpoint: fresh pixels have not reached the model for the latest result. For visual site edits, discover/load browser, open the approved page or local preview, use action=screenshot, and inspect it in the next model round. Earlier screenshots and source/DOM text do not verify the updated appearance. For other visual tasks, read the relevant image. If preview, permission, browser or vision input is unavailable, report the concrete limitation and finish with that limit; do not retry indefinitely or start unrelated services/models. This reminder is bounded.",
        )
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
mod tests {
    use super::*;
    #[test]
    fn visual_work_is_conditional_and_test_or_backend_edits_do_not_trigger_it() {
        assert!(useful("fix the spacing on this page"));
        assert!(useful("compare this picture with the rendered UI"));
        assert!(!useful("fix Docker image creation"));
        assert!(!useful("fix database query performance"));
        let mut review = Review::default();
        review.changed(Path::new("api.rs"));
        review.changed(Path::new("Button.test.tsx"));
        assert!(review.reminder((1, 0)).is_none());
        review.changed(Path::new("Button.tsx"));
        assert!(review.reminder((2, 0)).is_some());
        assert!(review.reminder((2, 0)).is_none());
    }
    #[test]
    fn only_rendered_pixels_delivered_after_latest_edit_satisfy_site_review() {
        let mut review = Review::new("fix the site layout");
        review.changed(Path::new("style.css"));
        review.captured((1, 0), false);
        review.delivered();
        assert!(
            review.reminder((1, 0)).is_some(),
            "a source image is not a rendered page"
        );
        review.captured((1, 0), true);
        assert!(
            review.missing((1, 0)),
            "capture and finish in one batch cannot inspect pixels"
        );
        review.delivered();
        assert!(!review.missing((1, 0)));
        assert!(
            review.reminder((2, 0)).is_some(),
            "later edits invalidate visual evidence"
        );
        assert!(
            review.reminder((2, 1)).is_some(),
            "opaque shell changes invalidate it too"
        );
    }
    #[test]
    fn missing_visual_evidence_is_disclosed_without_an_endless_completion_loop() {
        let mut review = Review::new("inspect the screenshot");
        assert!(review.reminder((0, 0)).is_some());
        assert!(review.reminder((0, 0)).is_none());
        assert!(
            review
                .summary("done".into(), (0, 0))
                .contains("not completed")
        );
        review.captured((0, 0), false);
        review.delivered();
        assert_eq!(review.summary("done".into(), (0, 0)), "done");
    }
}
