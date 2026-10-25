//! The in-session tree navigator loop (`/tree`): a picker over the journal's own entry tree.

use crate::app::shell::Shell;
use crate::storage::session::Session;
use crate::ui::tree_picker::{Action, Picker};
use anyhow::Result;
use crossterm::event::{Event, read};

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
        loop {
            let (cols, height) = self.term.size();
            let rows = picker.rows(cols as usize, height as usize, self.themes.current());
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
}
