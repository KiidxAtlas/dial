//! Plugin chrome: the header, footer, status and title slots plugins contribute. Plugin text is
//! untrusted, so every line is sanitized and width-fitted here as well as at the protocol edge. An
//! empty slot takes no row, so an idle plugin costs no layout.

use ratatui::text::{Line, Span};

use super::{
    text,
    theme::{Theme, Tok},
};

/// Most header rows painted above the composer, across all plugins.
pub const MAX_HEADER_ROWS: usize = 8;
/// Most footer rows painted below the composer, across all plugins.
pub const MAX_FOOTER_ROWS: usize = 8;

/// Where one contribution is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// Above the composer, in plugin order.
    Header,
    /// Below the composer, after the status row.
    Footer,
    /// One row below the composer that joins every plugin's status text.
    Status,
    /// Replaces the session title for the terminal title bar.
    Title,
}

/// The plugin contributions, already reduced to display text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PluginSlots {
    header: Vec<String>,
    footer: Vec<String>,
    status: Vec<String>,
    title: Option<String>,
}

impl PluginSlots {
    /// Builds the slots from `(placement, lines)` pairs in plugin order. Blank lines are dropped
    /// and every line is reduced to one sanitized line. The first title wins.
    pub fn collect<'a, I>(contributions: I) -> Self
    where
        I: IntoIterator<Item = (Placement, &'a [String])>,
    {
        let mut out = Self::default();
        for (placement, lines) in contributions {
            let clean: Vec<String> = lines
                .iter()
                .map(|line| text::one_line(line).trim().to_owned())
                .filter(|line| !line.is_empty())
                .collect();
            match placement {
                Placement::Header => out.header.extend(clean),
                Placement::Footer => out.footer.extend(clean),
                Placement::Status => out.status.extend(clean),
                Placement::Title => {
                    if out.title.is_none() {
                        out.title = clean.into_iter().next();
                    }
                }
            }
        }
        out.header.truncate(MAX_HEADER_ROWS);
        out.footer.truncate(MAX_FOOTER_ROWS);
        out
    }

    /// `true` when no plugin contributes anything, so the chrome keeps its exact layout.
    pub fn is_empty(&self) -> bool {
        self.header.is_empty()
            && self.footer.is_empty()
            && self.status.is_empty()
            && self.title.is_none()
    }

    /// The plugin title that replaces the session title, if any.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Rows above the composer's top border.
    pub fn header_rows(&self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        self.header
            .iter()
            .map(|line| row(line, width, theme))
            .collect()
    }

    /// Rows below the composer's bottom border, after [`Self::status_row`].
    pub fn footer_rows(&self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        self.footer
            .iter()
            .map(|line| row(line, width, theme))
            .collect()
    }

    /// The single status row, or `None` when no plugin shows status.
    pub fn status_row(&self, width: usize, theme: &Theme) -> Option<Line<'static>> {
        if self.status.is_empty() {
            return None;
        }
        let sep = if theme.ascii() { " | " } else { " · " };
        Some(row(&self.status.join(sep), width, theme))
    }
}

/// One muted row, indented to the composer's gutter and clipped to `width` cells.
fn row(line: &str, width: usize, theme: &Theme) -> Line<'static> {
    let fitted = text::ellipsize(line, width.saturating_sub(2), theme.ascii());
    Line::from(Span::styled(format!("  {fitted}"), theme.fg(Tok::Muted)))
}

#[cfg(test)]
mod tests;
