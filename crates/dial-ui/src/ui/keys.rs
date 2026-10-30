//! Remappable key bindings. Every non-text shortcut the UI reacts to is an [`Action`] with a stable id
//! (`app.model.select`), default chords, and a description. Users remap them in
//! `<dial home>/keybindings.toml`:
//!
//! ```toml
//! "app.model.select" = "Alt+M"                  # one chord
//! "tui.input.newline" = ["Shift+Enter", "Alt+Enter"]   # several
//! "app.history.search" = []                      # an empty list disables the action
//! # nested tables work too:  [app.model]  select = "Alt+M"
//! ```
//!
//! Components ask the [`Keymap`] — they never match raw key codes for these actions — so the footer
//! hints, `/hotkeys` and the behavior cannot disagree.
//!
//! Terminal limits (not Dial's): without the kitty keyboard protocol a terminal cannot report
//! `Shift+Enter`, `Ctrl+,` or `Ctrl+Shift+<key>`; the defaults therefore include fallbacks
//! (`Alt+Enter`, `Ctrl+4`). Labels use Option on macOS and Alt elsewhere; either spelling parses.
//! macOS terminals type symbols for Option chords unless Option is set to send Meta/Esc+, and
//! Windows Terminal claims `Alt+Enter` (fullscreen), so every default Option/Alt application chord
//! also has a Ctrl or function-key alternative (`Ctrl+Q`, `Ctrl+L`, `F2`–`F9`).
//! Model selection uses Ctrl+K on macOS so it works without changing terminal Option settings.
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::{collections::HashSet, fs, path::Path};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    Submit,
    FollowUp,
    Newline,
    Complete,
    Dismiss,
    Interrupt,
    ClearDraft,
    Exit,
    HistorySearch,
    EditExternal,
    PasteImage,
    VoiceInput,
    DisplayReset,
    CommandPalette,
    ModelSelect,
    ModelRoles,
    ModelFavoriteNext,
    ProviderSetup,
    Settings,
    ToolsExpand,
    ToolsToggle,
    ThinkingToggle,
    ApprovalCycle,
    SubagentsHub,
    SubagentsRecent,
    RateGood,
    RateBad,
    // navigation inside lists, menus and overlays
    SelectUp,
    SelectDown,
    SelectPageUp,
    SelectPageDown,
    SelectConfirm,
    SelectCancel,
    TabNext,
    TabPrev,
    SectionNext,
    SectionPrev,
    // text editing inside the composer and dialog fields
    EditorLeft,
    EditorRight,
    EditorWordLeft,
    EditorWordRight,
    EditorLineStart,
    EditorLineEnd,
    EditorJumpForward,
    EditorJumpBackward,
    EditorDeleteBackward,
    EditorDeleteForward,
    EditorDeleteWordBackward,
    EditorDeleteWordForward,
    EditorDeleteToLineStart,
    EditorDeleteToLineEnd,
    EditorYank,
    EditorYankPop,
    EditorUndo,
    EditorRedo,
}

impl Action {
    pub const ALL: &'static [Action] = &[
        Action::Submit,
        Action::FollowUp,
        Action::Newline,
        Action::Complete,
        Action::Dismiss,
        Action::Interrupt,
        Action::ClearDraft,
        Action::Exit,
        Action::HistorySearch,
        Action::EditExternal,
        Action::DisplayReset,
        Action::CommandPalette,
        Action::ModelSelect,
        Action::ModelRoles,
        Action::ModelFavoriteNext,
        Action::ProviderSetup,
        Action::Settings,
        Action::ToolsExpand,
        Action::ToolsToggle,
        Action::ThinkingToggle,
        Action::ApprovalCycle,
        Action::SubagentsHub,
        Action::SubagentsRecent,
        Action::RateGood,
        Action::RateBad,
        Action::SelectUp,
        Action::SelectDown,
        Action::SelectPageUp,
        Action::SelectPageDown,
        Action::SelectConfirm,
        Action::SelectCancel,
        Action::TabNext,
        Action::TabPrev,
        Action::SectionNext,
        Action::SectionPrev,
        Action::PasteImage,
        Action::VoiceInput,
        Action::EditorLeft,
        Action::EditorRight,
        Action::EditorWordLeft,
        Action::EditorWordRight,
        Action::EditorLineStart,
        Action::EditorLineEnd,
        Action::EditorJumpForward,
        Action::EditorJumpBackward,
        Action::EditorDeleteBackward,
        Action::EditorDeleteForward,
        Action::EditorDeleteWordBackward,
        Action::EditorDeleteWordForward,
        Action::EditorDeleteToLineStart,
        Action::EditorDeleteToLineEnd,
        Action::EditorYank,
        Action::EditorYankPop,
        Action::EditorUndo,
        Action::EditorRedo,
    ];

    /// Text-editing actions, dispatched by [`super::editor::Editor`] in this order.
    pub const EDITOR: &'static [Action] = &[
        Action::EditorLeft,
        Action::EditorRight,
        Action::EditorWordLeft,
        Action::EditorWordRight,
        Action::EditorLineStart,
        Action::EditorLineEnd,
        Action::EditorJumpForward,
        Action::EditorJumpBackward,
        Action::EditorDeleteBackward,
        Action::EditorDeleteForward,
        Action::EditorDeleteWordBackward,
        Action::EditorDeleteWordForward,
        Action::EditorDeleteToLineStart,
        Action::EditorDeleteToLineEnd,
        Action::EditorYank,
        Action::EditorYankPop,
        Action::EditorUndo,
        Action::EditorRedo,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Action::Submit => "tui.input.submit",
            Action::FollowUp => "tui.input.followup",
            Action::Newline => "tui.input.newline",
            Action::Complete => "tui.input.complete",
            Action::Dismiss => "tui.input.dismiss",
            Action::Interrupt => "app.interrupt",
            Action::ClearDraft => "app.clear",
            Action::Exit => "app.exit",
            Action::HistorySearch => "app.history.search",
            Action::EditExternal => "app.editor.external",
            Action::DisplayReset => "app.display.reset",
            Action::CommandPalette => "app.command.palette",
            Action::ModelSelect => "app.model.select",
            Action::ModelRoles => "app.model.roles",
            Action::ModelFavoriteNext => "app.model.favoriteNext",
            Action::ProviderSetup => "app.provider.setup",
            Action::Settings => "app.settings",
            Action::ToolsExpand => "app.tools.expand",
            Action::ToolsToggle => "app.tools.toggleVisibility",
            Action::ThinkingToggle => "app.thinking.toggle",
            Action::ApprovalCycle => "app.approval.cycle",
            Action::SubagentsHub => "app.subagents.hub",
            Action::SubagentsRecent => "app.subagents.recent",
            Action::RateGood => "app.turn.rateGood",
            Action::RateBad => "app.turn.rateBad",
            Action::SelectUp => "tui.select.up",
            Action::SelectDown => "tui.select.down",
            Action::SelectPageUp => "tui.select.pageUp",
            Action::SelectPageDown => "tui.select.pageDown",
            Action::SelectConfirm => "tui.select.confirm",
            Action::SelectCancel => "tui.select.cancel",
            Action::TabNext => "tui.tabs.next",
            Action::TabPrev => "tui.tabs.prev",
            Action::SectionNext => "tui.section.next",
            Action::SectionPrev => "tui.section.prev",
            Action::PasteImage => "app.clipboard.pasteImage",
            Action::VoiceInput => "app.voice.input",
            Action::EditorLeft => "tui.editor.cursorLeft",
            Action::EditorRight => "tui.editor.cursorRight",
            Action::EditorWordLeft => "tui.editor.cursorWordLeft",
            Action::EditorWordRight => "tui.editor.cursorWordRight",
            Action::EditorLineStart => "tui.editor.cursorLineStart",
            Action::EditorLineEnd => "tui.editor.cursorLineEnd",
            Action::EditorJumpForward => "tui.editor.jumpForward",
            Action::EditorJumpBackward => "tui.editor.jumpBackward",
            Action::EditorDeleteBackward => "tui.editor.deleteCharBackward",
            Action::EditorDeleteForward => "tui.editor.deleteCharForward",
            Action::EditorDeleteWordBackward => "tui.editor.deleteWordBackward",
            Action::EditorDeleteWordForward => "tui.editor.deleteWordForward",
            Action::EditorDeleteToLineStart => "tui.editor.deleteToLineStart",
            Action::EditorDeleteToLineEnd => "tui.editor.deleteToLineEnd",
            Action::EditorYank => "tui.editor.yank",
            Action::EditorYankPop => "tui.editor.yankPop",
            Action::EditorUndo => "tui.editor.undo",
            Action::EditorRedo => "tui.editor.redo",
        }
    }

    pub fn from_id(id: &str) -> Option<Action> {
        Action::ALL.iter().copied().find(|a| a.id() == id)
    }

    pub fn describe(self) -> &'static str {
        match self {
            Action::Submit => "Send the message",
            Action::FollowUp => "Queue a follow-up while busy; insert a newline when idle",
            Action::Newline => "Insert a newline",
            Action::Complete => "Accept the suggestion or completion",
            Action::Dismiss => "Dismiss suggestions",
            Action::Interrupt => "Stop the running turn",
            Action::ClearDraft => "Clear the draft (press again to exit when empty)",
            Action::Exit => "Exit when the draft is empty",
            Action::HistorySearch => "Search prompt history",
            Action::EditExternal => "Edit the draft in $VISUAL/$EDITOR",
            Action::DisplayReset => "Clear the screen and repaint",
            Action::CommandPalette => "Search slash commands",
            Action::ModelSelect => "Open the model selector",
            Action::ModelRoles => "Assign models to roles",
            Action::ModelFavoriteNext => "Switch to the next available favorite model",
            Action::ProviderSetup => "Set up providers and API keys",
            Action::Settings => "Open settings",
            Action::ToolsExpand => "Expand or collapse tool output",
            Action::ToolsToggle => "Show or hide tool activity",
            Action::ThinkingToggle => "Show or hide reasoning text",
            Action::ApprovalCycle => "Cycle approval mode (ask, write, auto, read-only)",
            Action::SubagentsHub => {
                "Open the worker hub: every worker, its activity, steering and cancel (Left twice on an empty composer also opens it)"
            }
            Action::SubagentsRecent => {
                "Open the most recent running worker's transcript; press again to cycle"
            }
            Action::RateGood => "Mark the last turn good (Teach this model)",
            Action::RateBad => "Mark the last turn bad (Teach this model)",
            Action::SelectUp => "Move up in lists",
            Action::SelectDown => "Move down in lists",
            Action::SelectPageUp => "Page up in lists",
            Action::SelectPageDown => "Page down in lists",
            Action::SelectConfirm => "Confirm the selection",
            Action::SelectCancel => "Close or go back",
            Action::TabNext => "Next tab",
            Action::TabPrev => "Previous tab",
            Action::SectionNext => "Next section",
            Action::SectionPrev => "Previous section",
            Action::PasteImage => "Attach the clipboard image (or copied image file)",
            Action::VoiceInput => "Speak a turn (press again to stop)",
            Action::EditorLeft => "Move the cursor left",
            Action::EditorRight => "Move the cursor right",
            Action::EditorWordLeft => "Move to the previous word",
            Action::EditorWordRight => "Move to the next word",
            Action::EditorLineStart => "Move to the start of the line",
            Action::EditorLineEnd => "Move to the end of the line",
            Action::EditorJumpForward => "Jump forward to the next typed character",
            Action::EditorJumpBackward => "Jump back to the previous typed character",
            Action::EditorDeleteBackward => "Delete the character before the cursor",
            Action::EditorDeleteForward => "Delete the character under the cursor",
            Action::EditorDeleteWordBackward => "Cut the previous word to the kill ring",
            Action::EditorDeleteWordForward => "Cut the next word to the kill ring",
            Action::EditorDeleteToLineStart => "Cut to the start of the line",
            Action::EditorDeleteToLineEnd => "Cut to the end of the line",
            Action::EditorYank => "Paste the last cut text",
            Action::EditorYankPop => "After a paste, cycle to older cut text",
            Action::EditorUndo => "Undo the last edit",
            Action::EditorRedo => "Redo the last undone edit",
        }
    }

    pub fn group(self) -> &'static str {
        match self {
            Action::Submit
            | Action::FollowUp
            | Action::Newline
            | Action::Complete
            | Action::Dismiss
            | Action::Interrupt
            | Action::ClearDraft
            | Action::Exit
            | Action::HistorySearch
            | Action::EditExternal
            | Action::PasteImage
            | Action::VoiceInput => "Input",
            Action::CommandPalette
            | Action::ModelSelect
            | Action::ModelRoles
            | Action::ModelFavoriteNext
            | Action::ProviderSetup
            | Action::Settings
            | Action::ApprovalCycle
            | Action::SubagentsHub
            | Action::SubagentsRecent
            | Action::RateGood
            | Action::RateBad => "Application",
            Action::ToolsExpand
            | Action::ToolsToggle
            | Action::ThinkingToggle
            | Action::DisplayReset => "Display",
            Action::EditorLeft
            | Action::EditorRight
            | Action::EditorWordLeft
            | Action::EditorWordRight
            | Action::EditorLineStart
            | Action::EditorLineEnd
            | Action::EditorJumpForward
            | Action::EditorJumpBackward
            | Action::EditorDeleteBackward
            | Action::EditorDeleteForward
            | Action::EditorDeleteWordBackward
            | Action::EditorDeleteWordForward
            | Action::EditorDeleteToLineStart
            | Action::EditorDeleteToLineEnd
            | Action::EditorYank
            | Action::EditorYankPop
            | Action::EditorUndo
            | Action::EditorRedo => "Editor",
            _ => "Navigation",
        }
    }

    pub fn defaults(self) -> &'static [&'static str] {
        match self {
            Action::Submit => &["Enter"],
            // Windows Terminal takes Alt+Enter for fullscreen; Ctrl+Q ("queue") always arrives.
            Action::FollowUp => &["Ctrl+Q", "Alt+Enter"],
            Action::Newline => &["Shift+Enter", "Ctrl+J"],
            Action::Complete => &["Tab", "Right"],
            Action::Dismiss => &["Esc"],
            Action::Interrupt => &["Esc", "Ctrl+C"],
            Action::ClearDraft => &["Ctrl+C"],
            Action::Exit => &["Ctrl+D"],
            Action::HistorySearch => &["Ctrl+R"],
            Action::EditExternal => &["Ctrl+G"],
            Action::DisplayReset => &["Ctrl+L", "Alt+L"],
            Action::CommandPalette => &["Ctrl+P", "Alt+K"],
            // Function keys back up Option chords, which macOS terminals often turn into symbols.
            Action::ModelSelect if cfg!(target_os = "macos") => &["Ctrl+K", "Alt+M", "F2"],
            Action::ModelSelect => &["Alt+M", "F2"],
            Action::ModelRoles => &["Alt+R", "F3"],
            Action::ModelFavoriteNext => &["Alt+]", "F8"],
            Action::ProviderSetup => &["Alt+P", "F4"],
            Action::Settings => &["Ctrl+,", "Ctrl+4"],
            Action::ToolsExpand => &["Ctrl+O"],
            Action::ToolsToggle => &["Alt+O", "F5"],
            Action::ThinkingToggle => &["Ctrl+T"],
            Action::ApprovalCycle => &["Shift+Tab"],
            Action::SubagentsHub => &["Alt+A", "F6"],
            Action::SubagentsRecent => &["Alt+Shift+A", "F7"],
            Action::RateGood => &["Alt+Up"],
            Action::RateBad => &["Alt+Down"],
            Action::SelectUp => &["Up", "Ctrl+P"],
            Action::SelectDown => &["Down", "Ctrl+N"],
            Action::SelectPageUp => &["PageUp"],
            Action::SelectPageDown => &["PageDown"],
            Action::SelectConfirm => &["Enter"],
            Action::SelectCancel => &["Esc"],
            Action::TabNext => &["Right"],
            Action::TabPrev => &["Left"],
            Action::SectionNext => &["Tab"],
            Action::SectionPrev => &["Shift+Tab"],
            // Windows Terminal binds Ctrl+V to its own text paste, which drops an image-only
            // clipboard; Alt+V attaches the image there, and voice keeps F9.
            Action::PasteImage if cfg!(windows) => &["Alt+V", "Ctrl+V"],
            Action::PasteImage => &["Ctrl+V"],
            Action::VoiceInput if cfg!(windows) => &["F9"],
            Action::VoiceInput => &["Alt+V", "F9"],
            Action::EditorLeft => &["Left", "Ctrl+B"],
            Action::EditorRight => &["Right", "Ctrl+F"],
            Action::EditorWordLeft => &["Alt+Left", "Ctrl+Left", "Alt+B"],
            Action::EditorWordRight => &["Alt+Right", "Ctrl+Right", "Alt+F"],
            Action::EditorLineStart => &["Home", "Ctrl+A"],
            Action::EditorLineEnd => &["End", "Ctrl+E"],
            // Legacy terminals report Ctrl+] as Ctrl+5 and Ctrl+_/Ctrl+- as Ctrl+7.
            Action::EditorJumpForward => &["Ctrl+]", "Ctrl+5"],
            Action::EditorJumpBackward => &["Ctrl+Alt+]", "Ctrl+Alt+5"],
            Action::EditorDeleteBackward => &["Backspace"],
            Action::EditorDeleteForward => &["Delete", "Ctrl+D"],
            Action::EditorDeleteWordBackward => &["Ctrl+W", "Alt+Backspace", "Ctrl+Backspace"],
            Action::EditorDeleteWordForward => &["Alt+Delete", "Alt+D", "Ctrl+Delete"],
            Action::EditorDeleteToLineStart => &["Ctrl+U"],
            Action::EditorDeleteToLineEnd => &["Ctrl+K"],
            Action::EditorYank => &["Ctrl+Y"],
            Action::EditorYankPop => &["Alt+Y"],
            Action::EditorUndo => &["Ctrl+-", "Ctrl+_", "Ctrl+7"],
            // Ctrl+6 (Ctrl+^) sits next to undo's Ctrl+7 and needs no Option key.
            Action::EditorRedo => &["Alt+-", "Ctrl+Alt+-", "Ctrl+6"],
        }
    }

    /// Actions in different contexts may share a chord (`Right` completes in the composer and switches
    /// tabs in a panel); within one context they may not.
    fn context(self) -> u8 {
        match self.group() {
            "Navigation" => 1,
            // Editing keys are consulted after application shortcuts, so they may reuse a chord
            // (`Ctrl+K` selects a model on macOS and cuts to line end in dialog fields).
            "Editor" => 2,
            _ => 0,
        }
    }

    /// Esc/Ctrl+C legitimately serve dismiss, clear and interrupt depending on state.
    fn overlap_exempt(self) -> bool {
        matches!(
            self,
            Action::Dismiss | Action::Interrupt | Action::ClearDraft
        )
    }
}

/// One key combination such as `Ctrl+P` or `Alt+Shift+Enter`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Chord {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

impl Chord {
    pub fn parse(text: &str) -> Result<Chord, String> {
        let mut mods = KeyModifiers::NONE;
        let mut code = None;
        // A literal plus key is written `Plus`, so `+` only ever separates parts.
        for part in text.split('+').map(str::trim) {
            let lower = part.to_ascii_lowercase();
            match lower.as_str() {
                "ctrl" | "control" => mods |= KeyModifiers::CONTROL,
                "alt" | "option" | "meta" => mods |= KeyModifiers::ALT,
                "shift" => mods |= KeyModifiers::SHIFT,
                "enter" | "return" => code = Some(KeyCode::Enter),
                "esc" | "escape" => code = Some(KeyCode::Esc),
                "tab" => code = Some(KeyCode::Tab),
                "space" => code = Some(KeyCode::Char(' ')),
                "backspace" => code = Some(KeyCode::Backspace),
                "delete" | "del" => code = Some(KeyCode::Delete),
                "up" => code = Some(KeyCode::Up),
                "down" => code = Some(KeyCode::Down),
                "left" => code = Some(KeyCode::Left),
                "right" => code = Some(KeyCode::Right),
                "home" => code = Some(KeyCode::Home),
                "end" => code = Some(KeyCode::End),
                "pageup" => code = Some(KeyCode::PageUp),
                "pagedown" => code = Some(KeyCode::PageDown),
                "plus" => code = Some(KeyCode::Char('+')),
                other if other.chars().count() == 1 => {
                    code = other.chars().next().map(KeyCode::Char)
                }
                other
                    if let Some(n) = other
                        .strip_prefix('f')
                        .and_then(|digits| digits.parse::<u8>().ok())
                        .filter(|n| (1..=24).contains(n)) =>
                {
                    code = Some(KeyCode::F(n));
                }
                "" => {
                    return Err(format!(
                        "chord {text:?} has an empty part (write a literal + as `Plus`)"
                    ));
                }
                other => return Err(format!("unknown key {other:?} in chord {text:?}")),
            }
        }
        code.map(|code| Chord { code, mods })
            .ok_or_else(|| format!("chord {text:?} names no key"))
    }

    pub fn matches(&self, key: &KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        let mut got = key.modifiers;
        let code = match key.code {
            // Shift+Tab arrives as BackTab on most terminals.
            KeyCode::BackTab => {
                got |= KeyModifiers::SHIFT;
                KeyCode::Tab
            }
            KeyCode::Char(c) if c.is_ascii_alphabetic() => KeyCode::Char(c.to_ascii_lowercase()),
            other => other,
        };
        let want = if let KeyCode::Char(c) = self.code {
            KeyCode::Char(c.to_ascii_lowercase())
        } else {
            self.code
        };
        // Plain printable characters ignore a stray Shift (uppercase letters, symbols).
        let ignore_shift = matches!(code, KeyCode::Char(_))
            && !self.mods.contains(KeyModifiers::CONTROL)
            && !self.mods.contains(KeyModifiers::ALT);
        let (a, b) = if ignore_shift {
            (got - KeyModifiers::SHIFT, self.mods - KeyModifiers::SHIFT)
        } else {
            (got, self.mods)
        };
        code == want && a == b
    }

    /// Matches only the key code of a release event; modifiers are ignored because the chord's
    /// modifier (e.g. Alt) is often already up when the key is released.
    pub fn matches_release(&self, key: &KeyEvent) -> bool {
        if key.kind != KeyEventKind::Release {
            return false;
        }
        match (key.code, self.code) {
            (KeyCode::Char(got), KeyCode::Char(want)) => got.eq_ignore_ascii_case(&want),
            (got, want) => got == want,
        }
    }

    /// Native shortcut label; Control remains Ctrl, never Command.
    pub fn display(&self) -> String {
        self.format_with_alt(if cfg!(target_os = "macos") {
            "Option"
        } else {
            "Alt"
        })
    }

    // Configuration spelling stays platform-independent.
    fn config_name(&self) -> String {
        self.format_with_alt("Alt")
    }

    fn format_with_alt(&self, alt: &str) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.mods.contains(KeyModifiers::CONTROL) {
            parts.push("Ctrl".into());
        }
        if self.mods.contains(KeyModifiers::ALT) {
            parts.push(alt.into());
        }
        if self.mods.contains(KeyModifiers::SHIFT) {
            parts.push("Shift".into());
        }
        parts.push(match self.code {
            KeyCode::Enter => "Enter".into(),
            KeyCode::Esc => "Esc".into(),
            KeyCode::Tab => "Tab".into(),
            KeyCode::Backspace => "Backspace".into(),
            KeyCode::Delete => "Delete".into(),
            KeyCode::Up => "Up".into(),
            KeyCode::Down => "Down".into(),
            KeyCode::Left => "Left".into(),
            KeyCode::Right => "Right".into(),
            KeyCode::Home => "Home".into(),
            KeyCode::End => "End".into(),
            KeyCode::PageUp => "PageUp".into(),
            KeyCode::PageDown => "PageDown".into(),
            KeyCode::F(n) => format!("F{n}"),
            KeyCode::Char(' ') => "Space".into(),
            KeyCode::Char('+') => "Plus".into(),
            KeyCode::Char(c) => c.to_ascii_uppercase().to_string(),
            other => format!("{other:?}"),
        });
        parts.join("+")
    }
}

/// One line of the `/hotkeys` reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HotkeyRow {
    pub group: &'static str,
    pub id: &'static str,
    /// Effective chords joined with ` / `, or `unbound`.
    pub keys: String,
    pub description: &'static str,
    pub customized: bool,
}

#[derive(Clone, Debug)]
pub struct Keymap {
    bindings: Vec<(Action, Vec<Chord>)>,
    customized: HashSet<Action>,
}

fn file_name() -> &'static str {
    "keybindings.toml"
}

impl Keymap {
    pub fn defaults() -> Keymap {
        Keymap {
            bindings: Action::ALL
                .iter()
                .map(|a| {
                    (
                        *a,
                        a.defaults()
                            .iter()
                            .filter_map(|c| Chord::parse(c).ok())
                            .collect(),
                    )
                })
                .collect(),
            customized: HashSet::new(),
        }
    }

    /// Defaults overlaid with `<user_dir>/keybindings.toml`. Returns human-readable warnings for
    /// anything ignored (unknown action, bad chord, conflict, unreadable file) instead of failing startup.
    pub fn load(user_dir: &Path) -> (Keymap, Vec<String>) {
        let path = user_dir.join(file_name());
        match fs::read_to_string(&path) {
            Ok(text) => match text.parse::<toml::Table>() {
                Ok(table) => Keymap::from_table(&table),
                Err(error) => (
                    Keymap::defaults(),
                    vec![format!(
                        "{}: {error}; using default key bindings",
                        path.display()
                    )],
                ),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (Keymap::defaults(), Vec::new())
            }
            Err(error) => (
                Keymap::defaults(),
                vec![format!(
                    "{}: {error}; using default key bindings",
                    path.display()
                )],
            ),
        }
    }

    /// Overlay a parsed `keybindings.toml` table (flat dotted keys or nested tables) on the defaults.
    pub fn from_table(table: &toml::Table) -> (Keymap, Vec<String>) {
        fn flatten(prefix: &str, table: &toml::Table, out: &mut Vec<(String, toml::Value)>) {
            for (key, value) in table {
                let id = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                match value {
                    // A table is a namespace only when it is not itself a recognizable action id's value.
                    toml::Value::Table(inner) => flatten(&id, inner, out),
                    other => out.push((id, other.clone())),
                }
            }
        }
        let mut entries = Vec::new();
        flatten("", table, &mut entries);
        let mut keymap = Keymap::defaults();
        let mut warnings = Vec::new();
        let mut wanted: Vec<(Action, Vec<Chord>)> = Vec::new();
        for (id, value) in entries {
            let Some(action) = Action::from_id(&id) else {
                warnings.push(format!("keybindings: unknown action {id:?} ignored"));
                continue;
            };
            let names: Vec<String> = match &value {
                toml::Value::String(s) => vec![s.clone()],
                toml::Value::Array(items) => {
                    let strings: Vec<String> = items
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect();
                    if strings.len() != items.len() {
                        warnings.push(format!("keybindings: {id} must be a chord string or a list of chord strings; ignored"));
                        continue;
                    }
                    strings
                }
                _ => {
                    warnings.push(format!("keybindings: {id} must be a chord string or a list of chord strings; ignored"));
                    continue;
                }
            };
            let mut chords = Vec::new();
            let mut bad = false;
            for name in &names {
                match Chord::parse(name) {
                    Ok(c) if !chords.contains(&c) => chords.push(c),
                    Ok(_) => {}
                    Err(error) => {
                        warnings.push(format!("keybindings: {id}: {error}; keeping the default"));
                        bad = true;
                    }
                }
            }
            if bad {
                continue;
            }
            wanted.push((action, chords));
        }
        wanted.sort_by_key(|(a, _)| Action::ALL.iter().position(|x| x == a));
        warnings.extend(keymap.apply(wanted));
        (keymap, warnings)
    }

    /// Install user bindings. Within one context, a chord claimed by two user-bound actions stays with the
    /// first (in `Action::ALL` order); a chord that a *default* binding also uses moves to the user's action.
    fn apply(&mut self, wanted: Vec<(Action, Vec<Chord>)>) -> Vec<String> {
        let mut warnings = Vec::new();
        // Explicit entries are a simultaneous keymap, not a sequence of transient
        // collisions with defaults that a later explicit entry will disable.
        for (action, _) in &wanted {
            if let Some((_, chords)) = self.bindings.iter_mut().find(|(other, _)| other == action) {
                chords.clear();
            }
        }
        for (action, chords) in wanted {
            let mut kept = Vec::new();
            for chord in chords {
                let clash = self.customized.iter().copied().find(|other| {
                    *other != action
                        && other.context() == action.context()
                        && !(other.overlap_exempt() && action.overlap_exempt())
                        && self.chords(*other).contains(&chord)
                });
                match clash {
                    Some(other) => warnings.push(format!(
                        "keybindings: {} is already bound to {}; it is not also bound to {}",
                        chord.display(),
                        other.id(),
                        action.id()
                    )),
                    None => kept.push(chord),
                }
            }
            for (other, existing) in &mut self.bindings {
                if *other == action
                    || self.customized.contains(other)
                    || other.context() != action.context()
                {
                    continue;
                }
                if other.overlap_exempt() && action.overlap_exempt() {
                    continue;
                }
                let before = existing.len();
                existing.retain(|c| !kept.contains(c));
                if existing.len() != before {
                    warnings.push(format!(
                        "keybindings: {} moved from {} to {}",
                        kept.iter()
                            .map(Chord::display)
                            .collect::<Vec<_>>()
                            .join("/"),
                        other.id(),
                        action.id()
                    ));
                }
            }
            if let Some(slot) = self.bindings.iter_mut().find(|(a, _)| *a == action) {
                slot.1 = kept;
            }
            self.customized.insert(action);
        }
        warnings
    }

    /// Replace `action`'s chords. Fails (changing nothing) on an unparsable chord or when another
    /// user-customized action in the same context already owns one; default bindings of other actions
    /// give way to the new one.
    pub fn rebind(&mut self, action: Action, chords: &[&str]) -> Result<(), String> {
        let mut parsed = Vec::new();
        for name in chords {
            let chord = Chord::parse(name)?;
            if !parsed.contains(&chord) {
                parsed.push(chord);
            }
        }
        for chord in &parsed {
            if let Some(other) = self.customized.iter().copied().find(|o| {
                *o != action
                    && o.context() == action.context()
                    && !(o.overlap_exempt() && action.overlap_exempt())
                    && self.chords(*o).contains(chord)
            }) {
                return Err(format!(
                    "{} is already bound to {}",
                    chord.display(),
                    other.id()
                ));
            }
        }
        self.customized.remove(&action);
        let warnings = self.apply(vec![(action, parsed)]);
        debug_assert!(warnings.iter().all(|w| w.contains("moved")), "{warnings:?}");
        Ok(())
    }

    /// Restore an action's default chords (other actions that gave up a chord to it do not get it back until
    /// the keymap is reloaded).
    pub fn reset(&mut self, action: Action) {
        self.customized.remove(&action);
        let defaults: Vec<Chord> = action
            .defaults()
            .iter()
            .filter_map(|c| Chord::parse(c).ok())
            .collect();
        if let Some(slot) = self.bindings.iter_mut().find(|(a, _)| *a == action) {
            slot.1 = defaults;
        }
    }

    pub fn is_customized(&self, action: Action) -> bool {
        self.customized.contains(&action)
    }

    /// Persist customized and displaced default bindings (atomic, owner-only). Refuses
    /// to overwrite a file that cannot be parsed, so a hand-edit with a typo is never destroyed.
    pub fn save(&self, user_dir: &Path) -> anyhow::Result<()> {
        let path = user_dir.join(file_name());
        if let Ok(existing) = fs::read_to_string(&path) {
            existing.parse::<toml::Table>().map_err(|e| {
                anyhow::anyhow!(
                    "{} is not valid TOML ({e}); fix or remove it before saving key bindings",
                    path.display()
                )
            })?;
        }
        let mut table = toml::Table::new();
        for action in Action::ALL.iter().filter(|action| {
            let defaults: Vec<_> = action
                .defaults()
                .iter()
                .filter_map(|name| Chord::parse(name).ok())
                .collect();
            self.customized.contains(action) || self.chords(**action) != defaults
        }) {
            let names: Vec<toml::Value> = self
                .chords(*action)
                .iter()
                .map(|c| toml::Value::String(c.config_name()))
                .collect();
            let value = if names.len() == 1 {
                names
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| toml::Value::Array(Vec::new()))
            } else {
                toml::Value::Array(names)
            };
            table.insert(action.id().to_owned(), value);
        }
        crate::platform::atomic::private_dir(user_dir)?;
        crate::config::settings_store::write_private(
            &path,
            toml::to_string_pretty(&table)?.as_bytes(),
        )
    }

    /// Apply an editor change only after validation and persistence succeed. The interactive editor
    /// never takes over another action's chord silently, or removes its own way to save and leave.
    pub fn edit_and_save(
        &mut self,
        user_dir: &Path,
        action: Action,
        chords: &[&str],
    ) -> anyhow::Result<()> {
        let mut next = self.clone();
        if chords == action.defaults() {
            next.reset(action);
        } else {
            next.rebind(action, chords).map_err(anyhow::Error::msg)?;
        }
        for other in Action::ALL.iter().copied().filter(|other| *other != action) {
            anyhow::ensure!(
                next.chords(other) == self.chords(other),
                "Shortcut is used by {}. Edit that action first.",
                other.describe()
            );
        }
        for required in [Action::SelectConfirm, Action::SelectCancel] {
            anyhow::ensure!(
                !next.chords(required).is_empty(),
                "Keep at least one shortcut for {} so panels remain usable.",
                required.describe()
            );
        }
        next.save(user_dir)?;
        *self = next;
        Ok(())
    }

    pub fn chords(&self, action: Action) -> &[Chord] {
        self.bindings
            .iter()
            .find(|(a, _)| *a == action)
            .map(|(_, c)| c.as_slice())
            .unwrap_or(&[])
    }
    pub fn matches(&self, action: Action, key: &KeyEvent) -> bool {
        let key = self.voice_key(action, *key);
        self.chords(action).iter().any(|c| c.matches(&key))
    }

    /// Like [`Keymap::matches`] for a shortcut checked ahead of other contexts (the composer
    /// consults application shortcuts before editing keys): a default chord gives way when the
    /// user bound the same key to an action in another context, so their binding is not shadowed.
    pub fn triggers(&self, action: Action, key: &KeyEvent) -> bool {
        self.matches(action, key)
            && (self.customized.contains(&action)
                || !self
                    .customized
                    .iter()
                    .any(|other| other.context() != action.context() && self.matches(*other, key)))
    }

    /// True when `key` releases one of `action`'s bound keys (see [`Chord::matches_release`]).
    pub fn release_matches(&self, action: Action, key: &KeyEvent) -> bool {
        let key = self.voice_key(action, *key);
        self.chords(action).iter().any(|c| c.matches_release(&key))
    }

    // Legacy macOS terminals send the layout's Option+V character, not an Alt modifier.
    // Keep this alias scoped to voice and its active Alt+V binding. Bracketed paste
    // bypasses key matching, so literal square roots can still be pasted into the draft.
    fn voice_key(&self, action: Action, mut key: KeyEvent) -> KeyEvent {
        if cfg!(target_os = "macos")
            && action == Action::VoiceInput
            && key.code == KeyCode::Char('√')
            && (key.modifiers - KeyModifiers::ALT).is_empty()
            && self
                .chords(action)
                .iter()
                .any(|chord| chord.code == KeyCode::Char('v') && chord.mods == KeyModifiers::ALT)
        {
            key.code = KeyCode::Char('v');
            key.modifiers = KeyModifiers::ALT;
        }
        key
    }

    /// Every action bound to this key (a chord may legitimately serve several, e.g. Esc).
    pub fn actions_for(&self, key: &KeyEvent) -> Vec<Action> {
        Action::ALL
            .iter()
            .copied()
            .filter(|a| self.matches(*a, key))
            .collect()
    }

    /// First chord for hints (`"Alt+M"`), or an empty string when the action is unbound.
    pub fn label(&self, action: Action) -> String {
        self.chords(action)
            .first()
            .map(Chord::display)
            .unwrap_or_default()
    }
    /// All chords joined for help text (`"Ctrl+, / Ctrl+4"`).
    pub fn labels(&self, action: Action) -> String {
        self.chords(action)
            .iter()
            .map(Chord::display)
            .collect::<Vec<_>>()
            .join(" / ")
    }

    /// The `/hotkeys` reference, in a stable order, reflecting remaps.
    pub fn hotkeys(&self) -> Vec<HotkeyRow> {
        Action::ALL
            .iter()
            .map(|a| HotkeyRow {
                group: a.group(),
                id: a.id(),
                description: a.describe(),
                customized: self.is_customized(*a),
                keys: if self.chords(*a).is_empty() {
                    "unbound".into()
                } else {
                    self.labels(*a)
                },
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
