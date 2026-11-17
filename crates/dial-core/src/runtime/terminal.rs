//! Frontend-neutral projection of an owned terminal emulator, never raw escape sequences.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cell {
    pub contents: String,
    /// Display cells occupied: zero for a wide character's continuation.
    pub width: u8,
    pub foreground: Color,
    pub background: Color,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Screen {
    pub rows: Vec<Vec<Cell>>,
    /// None when the child has hidden its cursor.
    pub cursor: Option<(u16, u16)>,
    pub bracketed_paste: bool,
    pub application_cursor: bool,
}
