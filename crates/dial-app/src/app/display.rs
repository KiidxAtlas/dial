//! Small text formatting helpers shared by the shell, overlays and command output.

use crate::agent;
use crate::config::types::Config;
use serde_json::Value;

pub(super) fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

pub(super) fn session_age(started: &str) -> String {
    let Some(stamp) = started.get(..19).and_then(crate::platform::time::parse_utc) else {
        return started.to_owned();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0);
    let seconds = now.saturating_sub(stamp).max(0) as u64;
    if seconds < 60 {
        "now".into()
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86400 {
        format!("{}h ago", seconds / 3600)
    } else {
        format!("{}d ago", seconds / 86400)
    }
}

/// An effective snapshot includes private policy and resolved limits omitted from persisted TOML.
pub(super) fn config_key(config: &Config) -> String {
    let mut table = toml::Table::try_from(config).expect("validated config serializes");
    table.insert(
        "effective_context_window".into(),
        i64::from(config.context_window).into(),
    );
    table.insert(
        "effective_chatgpt_accounts".into(),
        format!("{:?}", config.chatgpt_allowed_accounts).into(),
    );
    table.insert(
        "pinned_chatgpt_accounts".into(),
        format!("{:?}", config.chatgpt_pinned_accounts).into(),
    );
    table.to_string()
}

#[cfg(windows)]
pub(super) fn local_time(format: &str) -> String {
    crate::platform::time::local_strftime(format)
}

#[cfg(unix)]
/// `HH:MM`-style local time without a date library.
pub(super) fn local_time(format: &str) -> String {
    let Ok(format) = std::ffi::CString::new(format) else {
        return String::new();
    };
    // SAFETY: `localtime_r` and `strftime` write only into the zeroed `tm` and the bounded buffer.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return String::new();
        }
        let mut buffer = [0u8; 64];
        let written = libc::strftime(
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            format.as_ptr(),
            &tm,
        );
        String::from_utf8_lossy(&buffer[..written]).into_owned()
    }
}

/// Dark or light terminal background from `COLORFGBG` (`fg;bg`, bg index < 8 is dark); `None` = unknown.
pub(super) fn terminal_dark(value: Option<&str>) -> Option<bool> {
    let background = value?.rsplit(';').next()?.trim().parse::<u32>().ok()?;
    Some(background < 8 || background == 16)
}

pub(super) fn app_home_hint() -> String {
    if cfg!(windows) {
        return crate::config::paths::user_dir()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "set DIAL_HOME".into());
    }
    std::env::var("DIAL_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "~/.dial".into())
}

/// Plain words for the scheduler state recorded in `auto_status`.
pub(super) fn state_words(state: &str) -> String {
    match state {
        "" => "no cycle has run yet".into(),
        "idle" => "idle; the next cycle runs when due".into(),
        "queued" => "queued".into(),
        "running" => "running".into(),
        "paused" => "paused (daily compute budget reached)".into(),
        "stopped" => "stopped at your request".into(),
        "disabled" => "paused (automatic improvement is off)".into(),
        "failed" => "the last cycle failed".into(),
        other => other.replace('_', " "),
    }
}

/// One line per version: id, status, kind and parent. The live version is marked.
pub(super) fn version_line(version: &crate::config::types::Version, live: Option<i64>) -> String {
    let mut line = format!("v{} · {} · {}", version.id, version.status, version.kind);
    if let Some(parent) = version.parent {
        line.push_str(&format!(" · from v{parent}"));
    }
    if live == Some(version.id) {
        line.push_str(" · live");
    }
    line
}

/// The steps of a saved cycle report, one short line each.
pub(super) fn cycle_steps(report: &Value) -> Vec<String> {
    report["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|step| {
            let name = step["step"].as_str().unwrap_or("step").replace('_', " ");
            let detail = match &step["data"] {
                Value::String(text) => text.clone(),
                Value::Null => String::new(),
                other => other.to_string(),
            };
            if detail.is_empty() {
                name
            } else {
                format!("{name}: {}", agent::truncate(&detail, 120))
            }
        })
        .collect()
}

/// `32768` → `32,768`, for counts people read rather than parse.
pub(super) fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}
