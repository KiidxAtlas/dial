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
//! On macOS Terminal.app, Option chords need "Use Option as Meta key".
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
            Action::CommandPalette => "Search application actions",
            Action::ModelSelect => "Open the model selector",
            Action::ModelRoles => "Assign models to roles",
            Action::ModelFavoriteNext => "Switch to the next available favorite model",
            Action::ProviderSetup => "Set up providers and API keys",
            Action::Settings => "Open settings",
            Action::ToolsExpand => "Expand or collapse tool output",
            Action::ToolsToggle => "Show or hide tool activity",
            Action::ThinkingToggle => "Show or hide reasoning text",
            Action::ApprovalCycle => "Cycle approval mode (ask, auto, read-only)",
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
            Action::VoiceInput => "Dictate into the composer (press again to stop)",
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
            Action::FollowUp => &["Alt+Enter"],
            Action::Newline => &["Shift+Enter", "Ctrl+J"],
            Action::Complete => &["Tab", "Right"],
            Action::Dismiss => &["Esc"],
            Action::Interrupt => &["Esc", "Ctrl+C"],
            Action::ClearDraft => &["Ctrl+C"],
            Action::Exit => &["Ctrl+D"],
            Action::HistorySearch => &["Ctrl+R"],
            Action::EditExternal => &["Ctrl+G"],
            Action::DisplayReset => &["Alt+L"],
            Action::CommandPalette => &["Ctrl+P", "Alt+K"],
            Action::ModelSelect if cfg!(target_os = "macos") => &["Ctrl+K", "Alt+M"],
            Action::ModelSelect => &["Alt+M"],
            Action::ModelRoles => &["Alt+R"],
            Action::ModelFavoriteNext => &["Alt+]"],
            Action::ProviderSetup => &["Alt+P"],
            Action::Settings => &["Ctrl+,", "Ctrl+4"],
            Action::ToolsExpand => &["Ctrl+O"],
            Action::ToolsToggle => &["Alt+O"],
            Action::ThinkingToggle => &["Ctrl+T"],
            Action::ApprovalCycle => &["Shift+Tab"],
            Action::SubagentsHub => &["Alt+A"],
            Action::SubagentsRecent => &["Alt+Shift+A"],
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
            Action::PasteImage => &["Ctrl+V"],
            Action::VoiceInput => &["Alt+V"],
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
            Action::EditorRedo => &["Alt+-", "Ctrl+Alt+-"],
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
                    if other.starts_with('f')
                        && other[1..]
                            .parse::<u8>()
                            .is_ok_and(|n| (1..=24).contains(&n)) =>
                {
                    code = Some(KeyCode::F(other[1..].parse().unwrap_or(1)));
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
        crate::storage::settings_store::private_dir(user_dir)?;
        crate::storage::settings_store::write_private(
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
        self.chords(action).iter().any(|c| c.matches(key))
    }

    /// True when `key` releases one of `action`'s bound keys (see [`Chord::matches_release`]).
    pub fn release_matches(&self, action: Action, key: &KeyEvent) -> bool {
        self.chords(action).iter().any(|c| c.matches_release(key))
    }

    /// Every action bound to this key (a chord may legitimately serve several, e.g. Esc).
    #[cfg(test)]
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
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }
    fn table(text: &str) -> toml::Table {
        text.parse().unwrap()
    }

    fn native_alt(key: &str) -> String {
        format!(
            "{}+{key}",
            if cfg!(target_os = "macos") {
                "Option"
            } else {
                "Alt"
            }
        )
    }

    #[test]
    fn native_labels_preserve_aliases_modifiers_and_config_spelling() {
        for name in ["Alt+M", "Option+M", "Meta+M"] {
            let chord = Chord::parse(name).unwrap();
            assert_eq!(chord.display(), native_alt("M"));
            assert_eq!(chord.config_name(), "Alt+M");
            assert!(chord.matches(&key(KeyCode::Char('m'), KeyModifiers::ALT)));
            assert!(!chord.matches(&key(KeyCode::Char('m'), KeyModifiers::CONTROL)));
            assert_eq!(Chord::parse(&chord.display()).unwrap(), chord);
        }
        let combined = Chord::parse("Control+Option+Shift+Enter").unwrap();
        assert_eq!(
            combined.display(),
            format!("Ctrl+{}", native_alt("Shift+Enter"))
        );
        assert_eq!(Chord::parse("Control+K").unwrap().display(), "Ctrl+K");
        assert!(Chord::parse("Command+K").is_err());

        let (map, warnings) =
            Keymap::from_table(&table(r#""app.model.select" = ["Alt+M", "Option+M"]"#));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(map.chords(Action::ModelSelect).len(), 1);
        assert_eq!(map.label(Action::ModelSelect), native_alt("M"));
        assert_eq!(map.labels(Action::ModelSelect), native_alt("M"));
        assert_eq!(
            map.hotkeys()
                .iter()
                .find(|r| r.id == Action::ModelSelect.id())
                .unwrap()
                .keys,
            native_alt("M")
        );
    }

    #[test]
    fn every_action_has_a_unique_id_and_parsable_default_chords() {
        let mut ids = HashSet::new();
        for action in Action::ALL {
            assert!(ids.insert(action.id()), "duplicate {}", action.id());
            assert_eq!(Action::from_id(action.id()), Some(*action));
            for chord in action.defaults() {
                Chord::parse(chord).unwrap_or_else(|e| panic!("{}: {e}", action.id()));
            }
        }
    }

    #[test]
    fn model_shortcut_is_platform_specific_and_keeps_alt_compatibility() {
        let map = Keymap::defaults();
        assert_eq!(
            map.label(Action::ModelSelect),
            if cfg!(target_os = "macos") {
                "Ctrl+K"
            } else {
                "Alt+M"
            }
        );
        assert_eq!(
            map.matches(
                Action::ModelSelect,
                &key(KeyCode::Char('k'), KeyModifiers::CONTROL)
            ),
            cfg!(target_os = "macos")
        );
        assert!(map.matches(
            Action::ModelSelect,
            &key(KeyCode::Char('m'), KeyModifiers::ALT)
        ));
        assert!(!map.matches(
            Action::ModelSelect,
            &key(KeyCode::Enter, KeyModifiers::NONE)
        ));
        assert!(!map.matches(
            Action::ModelSelect,
            &key(KeyCode::Char('µ'), KeyModifiers::NONE)
        ));
    }

    #[test]
    fn chords_match_modifiers_exactly_and_backtab_is_shift_tab() {
        let alt_m = Chord::parse("Alt+M").unwrap();
        assert!(alt_m.matches(&key(KeyCode::Char('m'), KeyModifiers::ALT)));
        assert!(!alt_m.matches(&key(KeyCode::Char('m'), KeyModifiers::NONE)));
        assert!(!alt_m.matches(&key(
            KeyCode::Char('m'),
            KeyModifiers::ALT | KeyModifiers::CONTROL
        )));
        let shift_tab = Chord::parse("shift+tab").unwrap();
        assert!(
            shift_tab.matches(&key(KeyCode::BackTab, KeyModifiers::SHIFT))
                && shift_tab.matches(&key(KeyCode::BackTab, KeyModifiers::NONE))
        );
        assert!(
            !Chord::parse("Tab")
                .unwrap()
                .matches(&key(KeyCode::BackTab, KeyModifiers::SHIFT))
        );
        assert_eq!(alt_m.display(), native_alt("M"));
        let mut release = key(KeyCode::Char('m'), KeyModifiers::ALT);
        release.kind = KeyEventKind::Release;
        assert!(
            !alt_m.matches(&release),
            "key releases never trigger actions"
        );
    }

    #[test]
    fn chord_parsing_rejects_nonsense_and_round_trips_special_keys() {
        for bad in ["", "Ctrl+", "Ctrl+Banana", "Alt++", "F99", "Ctrl"] {
            assert!(Chord::parse(bad).is_err(), "{bad:?}");
        }
        for good in [
            "Ctrl+Plus",
            "Alt+Shift+Enter",
            "Ctrl+,",
            "F5",
            "Shift+Tab",
            "Alt+Space",
            "PageDown",
            "Ctrl+Alt+Delete",
        ] {
            let chord = Chord::parse(good).unwrap();
            assert_eq!(Chord::parse(&chord.display()).unwrap(), chord, "{good}");
        }
        assert!(
            Chord::parse("Ctrl+Plus")
                .unwrap()
                .matches(&key(KeyCode::Char('+'), KeyModifiers::CONTROL))
        );
    }

    #[test]
    fn user_file_overrides_adds_disables_and_reports_problems_without_failing() {
        let (map, warnings) = Keymap::from_table(&table(
            r#"
            "app.model.select" = "Ctrl+K"
            "tui.input.newline" = ["Shift+Enter", "Ctrl+Enter"]
            "app.history.search" = []
            "app.nonexistent" = "Ctrl+X"
            "app.tools.expand" = "Ctrl+Banana"
            "app.exit" = 42
            [app.provider]
            setup = "Alt+Shift+P"
        "#,
        ));
        assert!(map.matches(
            Action::ModelSelect,
            &key(KeyCode::Char('k'), KeyModifiers::CONTROL)
        ));
        assert!(
            !map.matches(
                Action::ModelSelect,
                &key(KeyCode::Char('m'), KeyModifiers::ALT)
            ),
            "the default is replaced, not added to"
        );
        assert_eq!(map.chords(Action::Newline).len(), 2);
        assert!(
            map.chords(Action::HistorySearch).is_empty(),
            "an empty list disables the action"
        );
        assert_eq!(map.label(Action::HistorySearch), "");
        assert_eq!(
            map.label(Action::ProviderSetup),
            native_alt("Shift+P"),
            "nested tables name the same action id"
        );
        assert_eq!(
            map.label(Action::ToolsExpand),
            "Ctrl+O",
            "an invalid chord keeps the default"
        );
        assert_eq!(map.label(Action::Exit), "Ctrl+D");
        let all = warnings.join("\n");
        assert!(
            all.contains("unknown action")
                && all.contains("Ctrl+Banana")
                && all.contains("app.exit"),
            "{all}"
        );
    }

    #[test]
    fn conflicts_resolve_deterministically_in_the_users_favor() {
        // A default binding of another action gives way to the user's choice.
        let (map, warnings) = Keymap::from_table(&table(r#""app.model.roles" = "Alt+M""#));
        assert_eq!(map.label(Action::ModelRoles), native_alt("M"));
        assert!(
            !map.matches(
                Action::ModelSelect,
                &key(KeyCode::Char('m'), KeyModifiers::ALT)
            ),
            "Alt+M moved off its default owner without removing other chords"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("moved from app.model.select")),
            "{warnings:?}"
        );
        // Two user bindings claiming one chord: the earlier action keeps it, the other is told.
        let (map, warnings) = Keymap::from_table(&table(
            "\"app.model.select\" = \"Ctrl+K\"\n\"app.settings\" = \"Ctrl+K\"",
        ));
        assert_eq!(map.label(Action::ModelSelect), "Ctrl+K");
        assert!(map.chords(Action::Settings).is_empty());
        assert!(
            warnings.iter().any(|w| w.contains("already bound")),
            "{warnings:?}"
        );
        // Different contexts may share a chord: Right completes in the composer and switches tabs in panels.
        let (map, _) = Keymap::from_table(&table(r#""tui.tabs.next" = "Right""#));
        assert!(
            map.matches(Action::Complete, &key(KeyCode::Right, KeyModifiers::NONE))
                && map.matches(Action::TabNext, &key(KeyCode::Right, KeyModifiers::NONE))
        );
        // Esc legitimately serves dismiss, interrupt and clear at once.
        let defaults = Keymap::defaults();
        let esc = key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(
            defaults.actions_for(&esc).contains(&Action::Dismiss)
                && defaults.actions_for(&esc).contains(&Action::Interrupt)
        );
    }

    #[test]
    fn rebind_reset_and_customization_tracking() {
        let mut map = Keymap::defaults();
        map.rebind(Action::ModelSelect, &["Ctrl+K"]).unwrap();
        assert!(map.is_customized(Action::ModelSelect));
        assert_eq!(map.label(Action::ModelSelect), "Ctrl+K");
        assert!(map.matches(
            Action::ModelSelect,
            &key(KeyCode::Char('k'), KeyModifiers::CONTROL)
        ));
        assert!(map.rebind(Action::ModelSelect, &["Ctrl+Banana"]).is_err());
        assert_eq!(
            map.label(Action::ModelSelect),
            "Ctrl+K",
            "a failed rebind changes nothing"
        );
        assert!(
            map.rebind(Action::Settings, &["Ctrl+K"])
                .unwrap_err()
                .contains("already bound to app.model.select")
        );
        map.rebind(Action::Settings, &["Alt+M"]).unwrap();
        assert_eq!(
            map.label(Action::Settings),
            native_alt("M"),
            "a free default chord can be taken"
        );
        map.reset(Action::ModelSelect);
        assert!(!map.is_customized(Action::ModelSelect));
        assert_eq!(
            map.label(Action::ModelSelect),
            Keymap::defaults().label(Action::ModelSelect)
        );
    }

    #[test]
    fn saved_bindings_round_trip_and_a_corrupt_file_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let mut map = Keymap::defaults();
        map.rebind(Action::ModelSelect, &["Ctrl+K"]).unwrap();
        map.rebind(Action::Newline, &["Alt+Enter", "Ctrl+Plus"])
            .unwrap();
        map.rebind(Action::HistorySearch, &[]).unwrap();
        map.save(dir.path()).unwrap();
        let (loaded, warnings) = Keymap::load(dir.path());
        assert!(warnings.is_empty(), "{warnings:?}");
        for action in [
            Action::ModelSelect,
            Action::Newline,
            Action::HistorySearch,
            Action::Settings,
        ] {
            assert_eq!(loaded.chords(action), map.chords(action), "{action:?}");
        }
        assert!(
            loaded.is_customized(Action::HistorySearch) && !loaded.is_customized(Action::Settings)
        );
        let text = fs::read_to_string(dir.path().join("keybindings.toml")).unwrap();
        let saved: toml::Table = text.parse().unwrap();
        assert_eq!(
            saved[Action::Newline.id()].as_array().unwrap()[0].as_str(),
            Some("Alt+Enter")
        );
        assert!(
            text.contains("app.model.select") && !text.contains("app.settings"),
            "only customized actions are written:\n{text}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(dir.path().join("keybindings.toml"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        fs::write(
            dir.path().join("keybindings.toml"),
            "app.model.select = [oops",
        )
        .unwrap();
        assert!(map.save(dir.path()).is_err());
        assert_eq!(
            fs::read_to_string(dir.path().join("keybindings.toml")).unwrap(),
            "app.model.select = [oops",
            "the user's file is untouched"
        );
        let (fallback, warnings) = Keymap::load(dir.path());
        assert_eq!(
            fallback.label(Action::ModelSelect),
            Keymap::defaults().label(Action::ModelSelect)
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("using default key bindings"))
        );
    }

    #[test]
    fn interactive_edits_persist_before_applying_and_reject_collisions_or_lockout() {
        let dir = tempfile::tempdir().unwrap();
        let mut map = Keymap::defaults();
        map.edit_and_save(dir.path(), Action::ModelSelect, &["Ctrl+K"])
            .unwrap();
        assert_eq!(map.label(Action::ModelSelect), "Ctrl+K");
        assert_eq!(
            Keymap::load(dir.path()).0.label(Action::ModelSelect),
            "Ctrl+K"
        );
        let path = dir.path().join("keybindings.toml");
        let before = fs::read_to_string(&path).unwrap();
        for (action, chords) in [
            (Action::HistorySearch, vec!["Ctrl+K"]),
            (Action::HistorySearch, vec!["Alt+P"]),
            (Action::SelectConfirm, vec![]),
            (Action::SelectCancel, vec![]),
            (Action::ModelSelect, vec!["Ctrl+Banana"]),
        ] {
            let old = map.chords(action).to_vec();
            assert!(map.edit_and_save(dir.path(), action, &chords).is_err());
            assert_eq!(map.chords(action), old);
            assert_eq!(fs::read_to_string(&path).unwrap(), before);
        }
        map.edit_and_save(dir.path(), Action::HistorySearch, &[])
            .unwrap();
        assert!(map.chords(Action::HistorySearch).is_empty());
        map.edit_and_save(
            dir.path(),
            Action::HistorySearch,
            Action::HistorySearch.defaults(),
        )
        .unwrap();
        assert!(
            !map.is_customized(Action::HistorySearch),
            "restoring defaults clears the modified marker"
        );
        assert_eq!(
            Keymap::load(dir.path()).0.label(Action::HistorySearch),
            "Ctrl+R"
        );
        assert!(
            !toml::from_str::<toml::Table>(&fs::read_to_string(&path).unwrap())
                .unwrap()
                .contains_key(Action::HistorySearch.id())
        );
        fs::write(&path, "not valid = [").unwrap();
        assert!(
            map.edit_and_save(dir.path(), Action::ModelSelect, &["Ctrl+Y"])
                .is_err()
        );
        assert_eq!(
            map.label(Action::ModelSelect),
            "Ctrl+K",
            "failed writes leave live shortcuts unchanged"
        );
        assert_eq!(fs::read_to_string(path).unwrap(), "not valid = [");
    }

    #[test]
    fn hotkeys_table_reflects_remaps_and_unbound_actions() {
        let mut map = Keymap::defaults();
        map.rebind(Action::HistorySearch, &[]).unwrap();
        map.rebind(Action::ModelSelect, &["Ctrl+K"]).unwrap();
        let rows = map.hotkeys();
        assert_eq!(rows.len(), Action::ALL.len());
        let find = |id: &str| rows.iter().find(|r| r.id == id).unwrap();
        assert_eq!(find("app.history.search").keys, "unbound");
        assert!(find("app.model.select").customized && find("app.model.select").keys == "Ctrl+K");
        assert_eq!(find("app.settings").keys, "Ctrl+, / Ctrl+4");
        assert!(!find("app.settings").customized);
        assert_eq!(find("tui.select.up").group, "Navigation");
    }

    #[test]
    fn editor_actions_form_their_own_context_without_default_collisions() {
        let grouped: Vec<Action> = Action::ALL
            .iter()
            .copied()
            .filter(|a| a.group() == "Editor")
            .collect();
        assert_eq!(grouped, Action::EDITOR);
        let map = Keymap::defaults();
        let mut seen = HashSet::new();
        for action in Action::EDITOR {
            assert_eq!(action.context(), 2);
            for chord in map.chords(*action) {
                assert!(
                    seen.insert(*chord),
                    "{} reused in the editor",
                    chord.display()
                );
            }
        }
        // A user may give an editing action a chord an application shortcut also uses.
        let (map, warnings) = Keymap::from_table(&table(r#""tui.editor.undo" = "Ctrl+P""#));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(map.matches(
            Action::EditorUndo,
            &key(KeyCode::Char('p'), KeyModifiers::CONTROL)
        ));
        assert!(map.matches(
            Action::CommandPalette,
            &key(KeyCode::Char('p'), KeyModifiers::CONTROL)
        ));
        assert!(map.matches(
            Action::EditorJumpForward,
            &key(KeyCode::Char('5'), KeyModifiers::CONTROL)
        ));
    }

    fn release(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        let mut event = KeyEvent::new(code, mods);
        event.kind = KeyEventKind::Release;
        event
    }

    #[test]
    fn voice_release_matches_bound_key_even_when_alt_is_already_up() {
        let map = Keymap::defaults();
        // Alt is often released before the letter; the release must still match.
        assert!(map.release_matches(
            Action::VoiceInput,
            &release(KeyCode::Char('v'), KeyModifiers::NONE)
        ));
        assert!(map.release_matches(
            Action::VoiceInput,
            &release(KeyCode::Char('V'), KeyModifiers::ALT)
        ));
        // Presses are not releases.
        assert!(!map.release_matches(
            Action::VoiceInput,
            &key(KeyCode::Char('v'), KeyModifiers::ALT)
        ));
        // Other keys do not release the voice action.
        assert!(!map.release_matches(
            Action::VoiceInput,
            &release(KeyCode::Char('x'), KeyModifiers::NONE)
        ));
    }
}
