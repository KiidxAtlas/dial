use crate::{
    llm::model_config::UiConfig,
    ui::{keys::Keymap, status::StatusData, theme::Theme},
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Frame, Terminal, backend::TestBackend, buffer::Buffer, style::Style, text::Line};

pub fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}
pub fn ctrl(c: char) -> Event {
    Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
}
pub fn ch(c: char) -> Event {
    key(KeyCode::Char(c))
}
pub fn typed(s: &str) -> Vec<Event> {
    s.chars().map(ch).collect()
}

pub fn status_preview(_: &UiConfig, _: u16) -> Line<'static> {
    Line::from("PREVIEW")
}

pub struct Rig {
    pub theme: Theme,
    pub keys: Keymap,
    pub status: StatusData,
}
impl Rig {
    pub fn new() -> Self {
        Rig {
            theme: Theme::dark(),
            keys: Keymap::defaults(),
            status: StatusData::default(),
        }
    }
    pub fn with_theme(theme: Theme) -> Self {
        Rig {
            theme,
            ..Rig::new()
        }
    }
}

pub fn render(width: u16, height: u16, draw: impl FnOnce(&mut Frame)) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(draw).unwrap();
    terminal.backend().buffer().clone()
}

pub fn rows(buffer: &Buffer) -> Vec<String> {
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect::<String>()
        })
        .collect()
}

pub fn screen(buffer: &Buffer) -> String {
    rows(buffer).join("\n")
}

/// Style of the first cell of `needle` found on the screen.
pub fn style_of(buffer: &Buffer, needle: &str) -> Option<Style> {
    for (y, row) in rows(buffer).iter().enumerate() {
        if let Some(byte) = row.find(needle) {
            let x = row[..byte].chars().count();
            return Some(buffer[(x as u16, y as u16)].style());
        }
    }
    None
}
