//! Paste handling: collapsed paste placeholders, clipboard images and voice transcripts.
use super::super::text;
use super::Composer;

/// Pastes longer than this many lines or characters collapse to a placeholder.
pub(super) const PASTE_COLLAPSE_LINES: usize = 20;

pub(super) const PASTE_COLLAPSE_CHARS: usize = 2000;

/// A file path as an `@` mention the reference parser reads back whole: paths containing
/// whitespace are quoted (with `'` when the path itself contains `"`).
pub(super) fn quote_mention(path: &str) -> String {
    if !path.contains(char::is_whitespace) {
        path.to_owned()
    } else if !path.contains('"') {
        format!("\"{path}\"")
    } else if !path.contains('\'') {
        format!("'{path}'")
    } else {
        path.to_owned()
    }
}

/// Replace each still-intact collapse placeholder with the pasted text it stands for, in one pass so
/// pasted text that looks like a placeholder is never expanded again.
pub(super) fn expand_pastes(draft: &str, pastes: &[(String, String)]) -> String {
    let mut out = String::with_capacity(draft.len());
    let mut rest = draft;
    while let Some(at) = rest.find('[') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        match pastes
            .iter()
            .find(|(placeholder, _)| tail.starts_with(placeholder.as_str()))
        {
            Some((placeholder, body)) => {
                out.push_str(body);
                rest = &tail[placeholder.len()..];
            }
            None => {
                out.push('[');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

impl Composer {
    /// Reads the platform clipboard image on a worker thread, so a slow clipboard tool (PowerShell
    /// cold start, a hung X server) never freezes the UI. [`Self::finish_clipboard_image`] attaches
    /// the result once it arrives.
    pub fn start_clipboard_image(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.clipboard_read.is_none(),
            "a clipboard image is already being read"
        );
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("clipboard-image".into())
            .spawn(move || {
                let _ = tx.send(crate::ui::clipboard::read_image());
            })
            .map_err(|error| anyhow::anyhow!("starting the clipboard reader: {error}"))?;
        self.clipboard_read = Some(rx);
        Ok(())
    }

    /// True while a clipboard image read is in flight; the shell polls faster meanwhile.
    pub fn clipboard_image_pending(&self) -> bool {
        self.clipboard_read.is_some()
    }

    /// Attaches a finished clipboard read and inserts its quoted path in the draft. Returns `None`
    /// while the read is still running.
    pub fn finish_clipboard_image(&mut self) -> Option<anyhow::Result<()>> {
        let rx = self.clipboard_read.as_ref()?;
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Empty) => return None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Err(anyhow::anyhow!("the clipboard reader stopped unexpectedly"))
            }
        };
        self.clipboard_read = None;
        Some(result.and_then(|bytes| self.attach_image_bytes(&bytes)))
    }

    pub(super) fn attach_image_bytes(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        use std::io::Write;
        anyhow::ensure!(!bytes.is_empty(), "clipboard contains no image");
        let mut file = tempfile::Builder::new()
            .prefix("dial-paste-")
            .suffix(".png")
            .tempfile()?;
        file.write_all(bytes)?;
        let owner = file.into_temp_path();
        let path = owner.to_string_lossy().into_owned();
        self.pasted_images.push(owner);
        // The image belongs to the draft, never to an open history-search query.
        self.search = None;
        self.choice = None;
        self.exit_armed = false;
        // A chip stands in for the temporary path; it expands to the quoted path on submit.
        self.paste_count += 1;
        let placeholder = format!("[Image #{}]", self.paste_count);
        self.editor.insert(&placeholder);
        self.pastes.push((placeholder, format!("\"{path}\"")));
        self.refresh();
        Ok(())
    }

    /// A collapsed paste or image chip that touches the cursor on the side `forward` deletes
    /// toward, as its byte range. Such chips are deleted whole, never character by character.
    pub(super) fn chip_at_cursor(&self, forward: bool) -> Option<(usize, usize)> {
        let text = &self.editor.text;
        let cursor = self.editor.cursor;
        self.pastes.iter().find_map(|(marker, _)| {
            if forward {
                text[cursor..]
                    .starts_with(marker.as_str())
                    .then(|| (cursor, cursor + marker.len()))
            } else {
                text[..cursor]
                    .ends_with(marker.as_str())
                    .then(|| (cursor - marker.len(), cursor))
            }
        })
    }

    /// Deletes a whole chip at the cursor as one undoable edit. False when there is none.
    pub(super) fn delete_chip(&mut self, forward: bool) -> bool {
        let Some((start, end)) = self.chip_at_cursor(forward) else {
            return false;
        };
        self.editor.delete_range(start, end);
        true
    }

    // -----------------------------------------------------------------------------------------
    // Input
    // -----------------------------------------------------------------------------------------

    /// Bracketed paste never submits. Long pastes collapse to `[Pasted text #n +X lines]` and are
    /// expanded when the draft is submitted.
    pub fn handle_paste(&mut self, pasted: &str) {
        let clean = text::sanitize(&pasted.replace("\r\n", "\n").replace('\r', "\n"));
        self.exit_armed = false;
        self.eof_armed = false;
        self.mention_edit = false;
        self.choice = None;
        if let Some(search) = &mut self.search {
            search.query.insert(&clean.replace('\n', " "));
            search.selected = 0;
            search.scroll = 0;
            self.refresh();
            return;
        }
        let lines = clean.trim_end_matches('\n').split('\n').count();
        if lines > PASTE_COLLAPSE_LINES || clean.chars().count() > PASTE_COLLAPSE_CHARS {
            self.paste_count += 1;
            let noun = if lines == 1 { "line" } else { "lines" };
            let placeholder = format!("[Pasted text #{} +{lines} {noun}]", self.paste_count);
            self.editor.insert(&placeholder);
            self.pastes.push((placeholder, clean));
        } else {
            self.editor.insert(&clean);
        }
        self.refresh();
    }

    /// Speech belongs to the draft, not an open history-search query.
    pub fn paste_voice(&mut self, transcript: &str) {
        self.search = None;
        self.handle_paste(transcript);
    }

    pub(super) fn prune_pastes(&mut self) {
        self.pastes.retain(|(marker, _)| {
            self.editor.references(marker)
                || self
                    .saved_draft
                    .as_ref()
                    .is_some_and(|draft| draft.references(marker))
        });
    }
}
