//! The in-session tree navigator loop (`/tree`): a picker over the journal's own entry tree. The
//! picker shows the conversation that leads to the selected node, rendered by the same transcript
//! the main view uses; the session still decides how the leaf moves once the user picks one.

use crate::app::shell::Shell;
use crate::storage::session::Session;
use crate::ui::input::read;
use crate::ui::transcript::{Replay, Transcript, TranscriptCtx};
use crate::ui::tree_picker::{Action, Picker, Preview};
use anyhow::Result;
use crossterm::event::Event;

/// Stored messages rendered for the context pane: the newest of the selected node's path.
const PREVIEW_MESSAGES: usize = 200;

impl Shell {
    /// Lets the user pick a node of `session`'s tree. `None` when cancelled or there is nothing to
    /// pick; the id of the chosen entry otherwise.
    pub(in crate::app) fn pick_tree_node(&mut self, session: &Session) -> Result<Option<String>> {
        let nodes = session.tree();
        if nodes.is_empty() {
            self.note("Tree", "No messages in this session yet.");
            return Ok(None);
        }
        let mut picker = Picker::new(nodes);
        // The preview of the node shown last, with the width it was rendered at.
        let mut shown: Option<(String, usize, Preview)> = None;
        loop {
            let (cols, height) = self.term.size();
            let width = Picker::preview_width(cols as usize);
            match picker.selected_id() {
                Some(id) => {
                    let fresh = matches!(&shown, Some((kept, w, _)) if kept == id && *w == width);
                    if !fresh {
                        let preview = self.tree_preview(session, id, width)?;
                        shown = Some((id.to_owned(), width, preview));
                    }
                }
                None => shown = None,
            }
            let empty = Preview::empty();
            let preview = shown.as_ref().map_or(&empty, |(_, _, preview)| preview);
            let rows = picker.rows(
                cols as usize,
                height as usize,
                self.themes.current(),
                preview,
            );
            self.paint(Some(rows))?;
            let event = read()?;
            if matches!(event, Event::Resize(..)) {
                self.term.on_resize();
            }
            match picker.handle(&event, height as usize) {
                Action::Cancel => return Ok(None),
                Action::Select(index) => return Ok(Some(picker.nodes[index].id.clone())),
                Action::None => {}
            }
        }
    }

    /// The conversation to entry `id`, rendered by the transcript the main view uses at `width`
    /// columns. Only stored messages on that path are shown, with tool results bound as on resume.
    fn tree_preview(&self, session: &Session, id: &str, width: usize) -> Result<Preview> {
        let messages = session.path_messages_to(id, PREVIEW_MESSAGES)?;
        let mut transcript = Transcript::new();
        let mut replay = Replay::default();
        for message in &messages {
            replay.push(&mut transcript, message);
        }
        transcript.finish_turn();
        for body in session.runtime().bodies("tool") {
            if let Some(call_id) = body.read(|node| node.call_id.clone()) {
                transcript.bind_tool(&call_id, body);
            }
        }
        let expand_key = self.keys.label(crate::ui::keys::Action::ToolsExpand);
        let ctx = TranscriptCtx {
            theme: self.themes.current(),
            ui: &self.ui,
            tick: 0,
            tools_expanded: self.tools_expanded,
            expand_key: &expand_key,
        };
        let width = u16::try_from(width).unwrap_or(u16::MAX);
        let (lines, _) = transcript.document(width, &ctx);
        Ok(Preview::new(lines, messages.len()))
    }
}
