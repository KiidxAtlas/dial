//! Option shortcuts from macOS terminals whose Option key types characters.
//!
//! Terminal.app, iTerm2 ("Option key sends: Normal") and the VS Code terminal send the character
//! the keyboard layout assigns to Option+key, with no Alt modifier: Option+K arrives as `˚` and
//! Option+M as `µ`. [`as_alt_chord`] reads such a character back as the Alt chord that typed it, so
//! `Option+K`, `Option+M` and the other Option shortcuts work without changing terminal settings.
//!
//! The table is the U.S. and ABC layouts' Option row. Other layouts type some of these characters
//! with plain keys (`ß` in German, `å` and `ø` in Scandinavian layouts, `ç` in French), so the
//! translation applies only while a U.S. or ABC layout is active ([`us_layout`]). Dead keys
//! (Option+E, I, N, U and `) type nothing on their own and cannot be recovered.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Option characters of the U.S./ABC layouts: (character, key, Shift held).
const US_OPTION: &[(char, char, bool)] = &[
    ('å', 'a', false),
    ('∫', 'b', false),
    ('ç', 'c', false),
    ('∂', 'd', false),
    ('ƒ', 'f', false),
    ('©', 'g', false),
    ('˙', 'h', false),
    ('∆', 'j', false),
    ('˚', 'k', false),
    ('¬', 'l', false),
    ('µ', 'm', false),
    ('ø', 'o', false),
    ('π', 'p', false),
    ('œ', 'q', false),
    ('®', 'r', false),
    ('ß', 's', false),
    ('†', 't', false),
    ('√', 'v', false),
    ('∑', 'w', false),
    ('≈', 'x', false),
    ('¥', 'y', false),
    ('Ω', 'z', false),
    ('¡', '1', false),
    ('™', '2', false),
    ('£', '3', false),
    ('¢', '4', false),
    ('∞', '5', false),
    ('§', '6', false),
    ('¶', '7', false),
    ('•', '8', false),
    ('ª', '9', false),
    ('º', '0', false),
    ('–', '-', false),
    ('≠', '=', false),
    ('“', '[', false),
    ('‘', ']', false),
    ('«', '\\', false),
    ('…', ';', false),
    ('æ', '\'', false),
    ('≤', ',', false),
    ('≥', '.', false),
    ('÷', '/', false),
    ('Å', 'a', true),
    ('ı', 'b', true),
    ('Ç', 'c', true),
    ('Î', 'd', true),
    ('Ï', 'f', true),
    ('˝', 'g', true),
    ('Ó', 'h', true),
    ('Ô', 'j', true),
    ('\u{F8FF}', 'k', true),
    ('Ò', 'l', true),
    ('Â', 'm', true),
    ('Ø', 'o', true),
    ('∏', 'p', true),
    ('Œ', 'q', true),
    ('‰', 'r', true),
    ('Í', 's', true),
    ('ˇ', 't', true),
    ('◊', 'v', true),
    ('„', 'w', true),
    ('˛', 'x', true),
    ('Á', 'y', true),
    ('¸', 'z', true),
];

/// The Alt chord a U.S./ABC Option character was typed with, as a key event of the same kind.
/// `None` for any other character, or when Control or Command was also held. A reported Shift
/// must agree with the table, so `Shift` plus an unshifted Option character is not a chord.
pub(super) fn as_alt_chord(key: &KeyEvent) -> Option<KeyEvent> {
    let KeyCode::Char(glyph) = key.code else {
        return None;
    };
    if !(key.modifiers - KeyModifiers::ALT - KeyModifiers::SHIFT).is_empty() {
        return None;
    }
    let &(_, base, shifted) = US_OPTION.iter().find(|(c, ..)| *c == glyph)?;
    if key.modifiers.contains(KeyModifiers::SHIFT) && !shifted {
        return None;
    }
    let mut modifiers = KeyModifiers::ALT;
    if shifted {
        modifiers |= KeyModifiers::SHIFT;
    }
    Some(KeyEvent {
        code: KeyCode::Char(base),
        modifiers,
        ..*key
    })
}

/// Whether the active keyboard layout is U.S. or ABC, read from the HIToolbox preferences and
/// cached briefly so a layout switch is noticed within seconds. False when it cannot be read,
/// and on other platforms.
pub(super) fn us_layout() -> bool {
    #[cfg(target_os = "macos")]
    {
        use std::sync::Mutex;
        use std::time::{Duration, Instant};
        static CACHE: Mutex<Option<(Instant, bool)>> = Mutex::new(None);
        let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, us)) = *cache
            && at.elapsed() < Duration::from_secs(2)
        {
            return us;
        }
        let us = std::env::var_os("HOME")
            .map(|home| {
                std::path::Path::new(&home).join("Library/Preferences/com.apple.HIToolbox.plist")
            })
            .and_then(|path| plist::Value::from_file(path).ok())
            .is_some_and(|prefs| selected_layout_is_us(&prefs));
        *cache = Some((Instant::now(), us));
        us
    }
    #[cfg(not(target_os = "macos"))]
    false
}

/// Whether the first keyboard layout in `AppleSelectedInputSources` is U.S. or ABC.
#[cfg(any(target_os = "macos", test))]
fn selected_layout_is_us(prefs: &plist::Value) -> bool {
    prefs
        .as_dictionary()
        .and_then(|prefs| prefs.get("AppleSelectedInputSources"))
        .and_then(plist::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(plist::Value::as_dictionary)
        .find(|source| {
            source
                .get("InputSourceKind")
                .and_then(plist::Value::as_string)
                == Some("Keyboard Layout")
        })
        .and_then(|layout| layout.get("KeyboardLayout Name"))
        .and_then(plist::Value::as_string)
        .is_some_and(|name| matches!(name, "U.S." | "ABC"))
}

#[cfg(test)]
mod tests;
