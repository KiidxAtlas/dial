//! Frozen review of exactly the visible saved overrides. Persistence belongs to the controller.
use super::*;

pub(super) struct Review {
    paths: Vec<String>,
    expected: BTreeMap<String, Value>,
    scope: Scope,
    label: String,
    details: Vec<String>,
    offset: usize,
    end: usize,
    confirmed: bool,
    waiting: bool,
    pub(super) display_ready: bool,
    error: Option<String>,
}

impl SettingsOverlay {
    /// The editing scope frozen when the review was opened.
    pub fn batch_scope(&self) -> Option<Scope> {
        self.bulk_review.as_ref().map(|review| review.scope)
    }

    /// True only after the rendered confirmation has emitted the pending batch event.
    pub fn batch_pending(&self) -> bool {
        self.bulk_review
            .as_ref()
            .is_some_and(|review| review.waiting)
    }

    /// Frozen raw values the controller can compare before committing a reviewed removal.
    pub fn batch_expected(&self) -> Option<&BTreeMap<String, Value>> {
        self.bulk_review.as_ref().map(|review| &review.expected)
    }

    /// Acknowledge an atomic batch operation; tuple order is effective, project, global.
    pub fn batch_settled(&mut self, result: Result<(Table, Table, Table), String>) {
        if !self
            .bulk_review
            .as_ref()
            .is_some_and(|review| review.waiting)
        {
            return;
        }
        match result {
            Ok((effective, project, global)) => {
                self.replace_values(effective, project, global);
                self.bulk_review = None;
                self.cursor = 0;
                self.scroll = 0;
                self.feedback = Some("Removed reviewed overrides".to_owned());
            }
            Err(error) => {
                if let Some(review) = &mut self.bulk_review {
                    review.waiting = false;
                    review.confirmed = false;
                    review.display_ready = false;
                    review.error = Some(text::one_line(&error));
                }
            }
        }
    }

    pub(super) fn open_bulk_review(&mut self) {
        if !self.pending.is_empty() || self.load_error.is_some() {
            self.feedback =
                Some("Wait for settings to settle before reviewing removals".to_owned());
            return;
        }
        let defs = crate::config::schema::registry();
        let mut expected = BTreeMap::new();
        let mut details = Vec::new();
        for row in self.visible() {
            let Row::Setting(index) = row else { continue };
            let def = &defs[index];
            if !resettable_path(def.path) {
                continue;
            }
            let Some(saved) = crate::config::schema::get(self.layer_ref(self.scope), def.path)
            else {
                continue;
            };
            expected.insert(def.path.to_owned(), saved.clone());
            let fallback = self
                .after_unset(def, self.scope)
                .map_or_else(|| "(not set)".to_owned(), |value| value_text(def, &value));
            details.push(format!("{} -> {fallback}", def.path));
        }
        if expected.is_empty() {
            self.feedback = Some("No visible saved overrides in this scope".to_owned());
            return;
        }
        // Never silently expand a filtered selection to a hidden partner.
        if expected.keys().any(|path| {
            crate::config::schema::pair_partner(path).is_some_and(|partner| {
                crate::config::schema::get(self.layer_ref(self.scope), partner).is_some()
                    && !expected.contains_key(partner)
            })
        }) {
            self.feedback = Some(
                "Embedding URL/model are removed together; show both saved paths before reviewing"
                    .to_owned(),
            );
            return;
        }
        self.bulk_review = Some(Review {
            paths: expected.keys().cloned().collect(),
            expected,
            scope: self.scope,
            label: self.editing_scope_label(),
            details,
            offset: 0,
            end: 0,
            confirmed: false,
            waiting: false,
            display_ready: false,
            error: None,
        });
    }

    pub(super) fn bulk_handle(&mut self, event: &Event, keys: &Keymap) -> OverlayEvent {
        let review = self.bulk_review.as_mut().expect("review open");
        if matches!(event, Event::Resize(..)) {
            review.display_ready = false;
        }
        let Event::Key(key) = event else {
            return OverlayEvent::None;
        };
        if !pressed(key) || review.waiting {
            return OverlayEvent::None;
        }
        if key.code == KeyCode::Esc || keys.matches(Action::SelectCancel, key) {
            self.bulk_review = None;
            return OverlayEvent::None;
        }
        if key.code == KeyCode::Enter && review.display_ready {
            review.display_ready = false;
            if review.confirmed {
                review.waiting = true;
                return OverlayEvent::UnsetMany {
                    paths: review.paths.clone(),
                    scope: review.scope,
                };
            }
            if review.offset >= review.end {
                review.confirmed = true;
            }
        } else if !review.confirmed {
            if keys.matches(Action::SelectDown, key) {
                review.offset = (review.offset + 1).min(review.end);
            } else if keys.matches(Action::SelectUp, key) {
                review.offset = review.offset.saturating_sub(1);
            } else if keys.matches(Action::SelectPageDown, key) || key.code == KeyCode::End {
                review.offset = review.end;
            } else if keys.matches(Action::SelectPageUp, key) || key.code == KeyCode::Home {
                review.offset = 0;
            }
            review.display_ready = false;
        }
        OverlayEvent::None
    }

    pub(super) fn bulk_compose(&mut self, w: usize, h: usize, ctx: &DrawCtx) -> Vec<Line<'static>> {
        let review = self.bulk_review.as_mut().expect("review open");
        review.display_ready = false;
        if w < 36 || h < 11 {
            return [
                "Removal review paused",
                "Enlarge to 36 columns / 11 rows",
                "Esc cancels; no changes saved",
            ]
            .into_iter()
            .take(h)
            .map(|s| Line::from(fit(vec![Span::raw(s)], w)))
            .collect();
        }
        let mut lines = vec![format!("Remove {} saved overrides?", review.paths.len())];
        // Wrap all content, including long project names, canonical paths and fallback values.
        lines.push(format!("Scope: {}", review.label));
        lines.push("Only listed paths; other scopes unchanged.".to_owned());
        lines.push("Embedding URL/model removal is paired.".to_owned());
        lines.push("CLI/env precedence remains unchanged.".to_owned());
        if let Some(error) = &review.error {
            lines.push(format!("Failed: {error}"));
        }
        if review.waiting {
            lines.push("Saving; waiting for controller…".to_owned());
        } else if review.confirmed {
            lines.push("Enter confirms removal; Esc cancels.".to_owned());
        } else {
            lines.extend(review.details.clone());
            lines.push("End of review. Enter continues; Esc cancels.".to_owned());
        }
        let wrapped: Vec<String> = lines.into_iter().flat_map(|line| wrap(&line, w)).collect();
        if review.confirmed || review.waiting {
            // A long project name or error must never hide the confirmation summary.
            review.display_ready = wrapped.len() <= h && !review.waiting;
            return wrapped
                .into_iter()
                .take(h)
                .map(|s| Line::styled(s, ctx.theme.fg(Tok::Text)))
                .collect();
        }
        let room = h.saturating_sub(1);
        review.end = wrapped.len().saturating_sub(room);
        review.offset = review.offset.min(review.end);
        review.display_ready = true;
        let mut visible: Vec<Line<'static>> = wrapped
            .into_iter()
            .skip(review.offset)
            .take(room)
            .map(|s| Line::styled(s, ctx.theme.fg(Tok::Text)))
            .collect();
        visible.push(hint_line(
            ctx.theme,
            &[
                format!("{} scroll", arrows(ctx.theme, Axis::Vertical)),
                "Enter next".to_owned(),
                "Esc cancel".to_owned(),
            ],
            w,
        ));
        visible
    }
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    for grapheme in text.graphemes(true) {
        let last = lines.last_mut().expect("one line");
        if text::width(last) + text::width(grapheme) > width {
            lines.push(grapheme.to_owned());
        } else {
            last.push_str(grapheme);
        }
    }
    lines
}
