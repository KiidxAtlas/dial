//! Value text, input parsing and the arrow-key adjustments for each setting kind.
use crate::config::schema::{Kind, SettingDef};
use crate::ui::text;
use toml::{Table, Value};

/// Pickers keep string identifiers, but automatic switches persist actual TOML booleans.
pub(super) fn choice_value(kind: Kind, choice: &str) -> Value {
    match (kind, choice) {
        (Kind::Bool | Kind::AutoBool, "true") => Value::Boolean(true),
        (Kind::Bool | Kind::AutoBool, "false") => Value::Boolean(false),
        _ => Value::String(choice.to_owned()),
    }
}

pub(super) fn fmt_float(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e15 {
        format!("{f:.1}")
    } else {
        format!("{f}")
    }
}

pub(super) fn value_text(def: &SettingDef, value: &Value) -> String {
    if def.sensitive() {
        return "(hidden)".into();
    }
    if matches!(def.kind, Kind::Toml) {
        return match value {
            Value::Table(items) => format!("{} entries · TOML", items.len()),
            Value::Array(items) => format!("{} items · TOML", items.len()),
            _ => "TOML value".into(),
        };
    }
    if let Some(meaning) = blank_meaning(def.description, value) {
        return text::one_line(&meaning);
    }
    let raw = match (&def.kind, value) {
        (Kind::AutoInt { .. } | Kind::AutoFloat { .. }, Value::String(s)) if s == "auto" => {
            "Auto".into()
        }
        (Kind::AutoBool, Value::String(s)) if s == "auto" => "auto".to_owned(),
        (Kind::Bool | Kind::AutoBool, Value::Boolean(b)) => {
            if *b { "on" } else { "off" }.to_owned()
        }
        (Kind::Enum(options) | Kind::Choice(options), Value::String(s)) => options
            .iter()
            .find(|(v, _)| v == s)
            .map_or_else(|| s.clone(), |(_, label)| (*label).to_owned()),
        (_, Value::String(s)) if s.is_empty() => "not set".to_owned(),
        (_, Value::String(s)) => s.clone(),
        (_, Value::Integer(n)) => n.to_string(),
        (_, Value::Float(f)) => fmt_float(*f),
        (_, Value::Boolean(b)) => b.to_string(),
        (_, Value::Array(items)) if items.is_empty() => "(none)".to_owned(),
        (_, Value::Array(items)) => items
            .iter()
            .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
            .collect::<Vec<_>>()
            .join(", "),
        (_, other) => other.to_string(),
    };
    text::one_line(&raw)
}

/// What a zero or empty value does, in the setting's own words: its description's
/// "0 means unlimited", "0 disables", "Empty uses python3 from PATH" or "empty disables".
/// `None` when the value is not blank or the description does not say.
pub(super) fn blank_meaning(description: &str, value: &Value) -> Option<String> {
    let (shown, subjects) = match value {
        Value::Integer(0) => ("0", &["0"][..]),
        Value::String(s) if s.is_empty() => ("not set", &["empty", "blank"][..]),
        Value::Array(items) if items.is_empty() => ("none", &["empty"][..]),
        _ => return None,
    };
    let lower = description.to_ascii_lowercase();
    subjects.iter().find_map(|subject| {
        // A clause starts the description or follows a sentence or clause break.
        let at = [". ", "; ", ", ", "("]
            .iter()
            .filter_map(|lead| {
                lower
                    .find(&format!("{lead}{subject} "))
                    .map(|i| i + lead.len())
            })
            .chain(lower.starts_with(&format!("{subject} ")).then_some(0))
            .min()?;
        let rest = &description[at + subject.len() + 1..];
        let end = rest.find(['.', ';', ',', '(']).unwrap_or(rest.len());
        let clause = rest[..end].trim();
        let meaning = if let Some(what) = clause.strip_prefix("means ") {
            what.trim()
        } else if clause.starts_with("disables") {
            "off"
        } else {
            clause.strip_prefix("uses ")?.trim()
        };
        // A pronoun leans on the sentence it came from ("Empty uses that default").
        let meaning = if meaning.starts_with("that ") || meaning.starts_with("this ") {
            "default"
        } else {
            meaning
        };
        Some(if meaning.is_empty() || meaning == shown {
            shown.to_owned()
        } else {
            format!("{shown} ({meaning})")
        })
    })
}

pub(super) fn setting_edit_text(def: &SettingDef, value: &Value) -> String {
    if matches!(def.kind, Kind::Toml) {
        value.to_string()
    } else {
        edit_text(value)
    }
}

pub(super) fn edit_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Integer(n) => n.to_string(),
        Value::Float(f) => fmt_float(*f),
        Value::Array(items) => items
            .iter()
            .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
    }
}

pub(super) fn input_hint(kind: Kind) -> String {
    match kind {
        Kind::AutoInt { min, max, .. } => format!("Auto or a whole number from {min} to {max}."),
        Kind::AutoFloat { min, max, .. } => format!(
            "Auto or a number from {} to {}.",
            fmt_float(min),
            fmt_float(max)
        ),
        Kind::Int { min, max, .. } => format!("Whole number from {min} to {max}."),
        Kind::Float { min, max, .. } => {
            format!("Number from {} to {}.", fmt_float(min), fmt_float(max))
        }
        Kind::StringList => "Comma-separated list.".to_owned(),
        Kind::Toml => "One TOML value: { name = { program = \"tool\" } } or [{ name = \"check\" }]. Enter saves; Esc cancels; Delete in browse resets.".to_owned(),
        Kind::Table => {
            "TOML inline table, for example { \"provider/model\" = \"80%\" }.".to_owned()
        }
        _ => "Text.".to_owned(),
    }
}

/// Parse and range-check typed input for `kind`.
pub(super) fn parse_input(kind: Kind, input: &str) -> Result<Value, String> {
    let input = input.trim();
    match kind {
        Kind::AutoInt { .. } | Kind::AutoFloat { .. } if input.eq_ignore_ascii_case("auto") => {
            Ok(Value::String("auto".into()))
        }
        Kind::Int { min, max, .. } | Kind::AutoInt { min, max, .. } => {
            let n: i64 = input
                .parse()
                .map_err(|_| format!("Enter a whole number between {min} and {max}"))?;
            if (min..=max).contains(&n) {
                Ok(Value::Integer(n))
            } else {
                Err(format!("Must be between {min} and {max}"))
            }
        }
        Kind::Float { min, max, .. } | Kind::AutoFloat { min, max, .. } => {
            let f: f64 = input
                .parse()
                .ok()
                .filter(|f: &f64| f.is_finite())
                .ok_or_else(|| {
                    format!(
                        "Enter a number between {} and {}",
                        fmt_float(min),
                        fmt_float(max)
                    )
                })?;
            if f >= min && f <= max {
                Ok(Value::Float(f))
            } else {
                Err(format!(
                    "Must be between {} and {}",
                    fmt_float(min),
                    fmt_float(max)
                ))
            }
        }
        Kind::StringList => Ok(Value::Array(
            input
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| Value::String(s.to_owned()))
                .collect(),
        )),
        Kind::Toml => {
            if input.len() > 256 * 1024 {
                return Err("Settings value exceeds 256 KiB".into());
            }
            let mut table: Table = toml::from_str(&format!("value = {input}"))
                .map_err(|_| "Enter one valid TOML value (inline table or array)".to_owned())?;
            let value = table.remove("value").ok_or("Enter a TOML value")?;
            if !table.is_empty() {
                return Err("Enter only one TOML value".into());
            }
            Ok(value)
        }
        Kind::Table => {
            if input.len() > 16 * 1024 {
                return Err("Settings table exceeds 16 KiB".into());
            }
            let mut table: Table = toml::from_str(&format!("value = {input}"))
                .map_err(|_| "Enter a valid TOML inline table".to_owned())?;
            let value = table.remove("value").ok_or("Enter a TOML inline table")?;
            if !value.is_table() || !table.is_empty() {
                return Err("Enter one TOML inline table".into());
            }
            Ok(value)
        }
        _ => Ok(Value::String(input.to_owned())),
    }
}

/// Step a numeric buffer up (`dir = 1`) or down.
pub(super) fn adjust(kind: Kind, buffer: &str, dir: i64) -> Option<String> {
    match kind {
        Kind::AutoInt { min, .. } if buffer.trim().eq_ignore_ascii_case("auto") => {
            Some(if dir > 0 {
                min.to_string()
            } else {
                "auto".into()
            })
        }
        Kind::AutoInt { min, .. } if dir < 0 && buffer.trim().parse::<i64>().ok() == Some(min) => {
            Some("auto".into())
        }
        Kind::Int { min, max, step } | Kind::AutoInt { min, max, step } => {
            let n = buffer.trim().parse::<i64>().unwrap_or(min);
            Some(
                n.saturating_add(step.saturating_mul(dir))
                    .clamp(min, max)
                    .to_string(),
            )
        }
        Kind::AutoFloat { min, .. } if buffer.trim().eq_ignore_ascii_case("auto") => {
            Some(if dir > 0 {
                fmt_float(min)
            } else {
                "auto".into()
            })
        }
        Kind::AutoFloat { min, .. }
            if dir < 0
                && buffer
                    .trim()
                    .parse::<f64>()
                    .ok()
                    .is_some_and(|value| value <= min) =>
        {
            Some("auto".into())
        }
        Kind::Float { min, max, step } | Kind::AutoFloat { min, max, step } => {
            let f = buffer
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|f| f.is_finite())
                .unwrap_or(min);
            Some(fmt_float(
                ((f + step * dir as f64).clamp(min, max) * 1e6).round() / 1e6,
            ))
        }
        _ => None,
    }
}
