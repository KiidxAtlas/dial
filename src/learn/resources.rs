//! When background improvement may use the machine.
//!
//! Training and evaluation are heavy, so a background cycle starts only when the machine is
//! otherwise free: no coding turn for a while, on mains power, not in Low Power Mode, not thermally
//! throttled, and under a daily compute budget that is also enforced while a cycle runs. All
//! limits are in `[improve]`; forced cycles (`dial improve cycle`, `/improve start`) bypass the
//! start-time checks but still honour the daily budget only if you set one.
use crate::config::types::ImproveConfig;
use crate::storage::settings_store;
use anyhow::{Context, Result, bail};
use std::{
    path::Path,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

/// Seconds since the epoch.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// `None` when power source is unknown (desktops, other platforms): never blocks.
    pub on_ac: Option<bool>,
    pub low_power: bool,
    pub throttled: bool,
    /// Seconds since the last coding turn asked for compute; `None` if none ever did.
    pub idle_secs: Option<u64>,
    pub used_today_secs: u64,
    /// The persisted daily usage record is corrupt; a budgeted machine must not start background work.
    pub usage_unreadable: bool,
}

impl Snapshot {
    pub fn permissive() -> Self {
        Self {
            on_ac: None,
            low_power: false,
            throttled: false,
            idle_secs: None,
            used_today_secs: 0,
            usage_unreadable: false,
        }
    }
}

/// Why a cycle should wait, or `None` if it may start.
pub fn blocker(config: &ImproveConfig, snapshot: &Snapshot) -> Option<String> {
    if config.require_ac_power && snapshot.on_ac == Some(false) {
        return Some("running on battery".into());
    }
    if snapshot.low_power {
        return Some("Low Power Mode is on".into());
    }
    if snapshot.throttled {
        return Some("the machine is thermally throttled".into());
    }
    if let Some(idle) = snapshot.idle_secs {
        let need = u64::from(config.idle_minutes) * 60;
        if idle < need {
            return Some(format!(
                "a coding turn ran {}m ago (waits for {}m idle)",
                idle / 60,
                config.idle_minutes
            ));
        }
    }
    if snapshot.usage_unreadable && config.daily_compute_minutes > 0 {
        return Some(
            "compute-usage.json is unreadable; repair or remove it to resume background compute"
                .into(),
        );
    }
    if let Some(left) = budget_left(config, snapshot.used_today_secs)
        && left == 0
    {
        return Some(format!(
            "daily compute budget of {}m is used up",
            config.daily_compute_minutes
        ));
    }
    None
}

/// Seconds of today's compute budget left; `None` when unlimited.
pub fn budget_left(config: &ImproveConfig, used_secs: u64) -> Option<u64> {
    (config.daily_compute_minutes > 0)
        .then(|| (u64::from(config.daily_compute_minutes) * 60).saturating_sub(used_secs))
}

pub fn parse_power_source(pmset_batt: &str) -> Option<bool> {
    if pmset_batt.contains("AC Power") {
        Some(true)
    } else if pmset_batt.contains("Battery Power") {
        Some(false)
    } else {
        None
    }
}

pub fn parse_low_power(pmset: &str) -> bool {
    pmset.lines().any(|l| {
        let l = l.trim();
        l.starts_with("lowpowermode") && l.split_whitespace().nth(1) == Some("1")
    })
}

/// `pmset -g therm` reports `CPU_Speed_Limit = N` only while throttled.
pub fn parse_throttled(therm: &str) -> bool {
    therm
        .lines()
        .filter_map(|l| l.trim().strip_prefix("CPU_Speed_Limit"))
        .any(|rest| {
            rest.trim_start_matches([' ', '='])
                .trim()
                .parse::<u32>()
                .is_ok_and(|limit| limit < 100)
        })
}

fn pmset(args: &[&str]) -> String {
    Command::new("pmset")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// The current state of the machine. Unit tests never probe real hardware.
pub fn probe(user_dir: &Path) -> Snapshot {
    if cfg!(test) {
        return Snapshot::permissive();
    }
    let mac = cfg!(target_os = "macos");
    Snapshot {
        on_ac: if mac {
            parse_power_source(&pmset(&["-g", "batt"]))
        } else {
            None
        },
        low_power: mac && parse_low_power(&pmset(&["-g"])),
        throttled: mac && parse_throttled(&pmset(&["-g", "therm"])),
        idle_secs: idle_secs(user_dir),
        used_today_secs: used_today(user_dir),
        usage_unreadable: read_usage_checked(user_dir, now()).is_err(),
    }
}

fn marker(user_dir: &Path) -> std::path::PathBuf {
    user_dir.join("foreground-last")
}

/// Called whenever a coding turn asks for compute.
pub fn note_foreground(user_dir: &Path) {
    let _ = std::fs::create_dir_all(user_dir);
    let _ = settings_store::write_private(&marker(user_dir), now().to_string().as_bytes());
}

pub fn idle_secs(user_dir: &Path) -> Option<u64> {
    let last: u64 = std::fs::read_to_string(marker(user_dir))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(now().saturating_sub(last))
}

fn usage_path(user_dir: &Path) -> std::path::PathBuf {
    user_dir.join("compute-usage.json")
}

fn day(at: u64) -> u64 {
    at / 86_400
}

fn read_usage(user_dir: &Path, at: u64) -> u64 {
    read_usage_checked(user_dir, at).unwrap_or(0)
}

/// Today's recorded usage. A missing record or one for another day counts as zero; a corrupt or
/// unreadable record is an error, so writers never overwrite it and admission can fail closed.
fn read_usage_checked(user_dir: &Path, at: u64) -> Result<u64> {
    let text = match std::fs::read_to_string(usage_path(user_dir)) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    let record: serde_json::Value =
        serde_json::from_str(&text).context("compute usage record is corrupt")?;
    let (Some(recorded_day), Some(secs)) = (record["day"].as_u64(), record["secs"].as_u64()) else {
        bail!("compute usage record is corrupt");
    };
    Ok(if recorded_day == day(at) { secs } else { 0 })
}

pub fn used_today(user_dir: &Path) -> u64 {
    read_usage(user_dir, now())
}

/// Today's usage, or an error when the record is corrupt or unreadable (admission fails closed).
pub fn used_today_checked(user_dir: &Path) -> Result<u64> {
    read_usage_checked(user_dir, now())
}

/// Add background compute time to today's total.
pub fn record_usage(user_dir: &Path, secs: u64) -> Result<()> {
    record_usage_at(user_dir, secs, now())
}

fn record_usage_at(user_dir: &Path, secs: u64, at: u64) -> Result<()> {
    if secs == 0 {
        return Ok(());
    }
    std::fs::create_dir_all(user_dir)?;
    // The stable settings lock covers the whole read-modify-write, independent of compute_locks.
    let _lock = settings_store::locked(&usage_path(user_dir))?;
    let total = read_usage_checked(user_dir, at)? + secs;
    settings_store::write_private(
        &usage_path(user_dir),
        serde_json::json!({"day": day(at), "secs": total})
            .to_string()
            .as_bytes(),
    )?;
    Ok(())
}

/// One line for status displays; empty when nothing blocks.
pub fn describe(config: &ImproveConfig, user_dir: &Path) -> Option<String> {
    blocker(config, &probe(user_dir))
}

/// Wall-clock time per cycle phase, so it is visible whether generation, training or evaluation
/// is what holds the machine.
#[derive(Default)]
pub struct Timing {
    current: Option<(String, std::time::Instant)>,
    totals: std::collections::BTreeMap<String, f64>,
}

/// `evaluation check 12/60 (v3)` and `evaluation check 13/60 (v3)` are the same phase.
pub fn stage_key(stage: &str) -> String {
    let cut = stage
        .find(|c: char| c.is_ascii_digit() || c == '(' || c == ':' || c == '#')
        .unwrap_or(stage.len());
    let key = stage[..cut].trim().to_lowercase();
    if key.is_empty() { "other".into() } else { key }
}

impl Timing {
    pub fn enter(&mut self, stage: &str) {
        let key = stage_key(stage);
        if self.current.as_ref().is_some_and(|(k, _)| *k == key) {
            return;
        }
        self.close();
        self.current = Some((key, std::time::Instant::now()));
    }
    fn close(&mut self) {
        if let Some((key, since)) = self.current.take() {
            *self.totals.entry(key).or_default() += since.elapsed().as_secs_f64();
        }
    }
    /// Seconds per phase, rounded to 0.1.
    pub fn finish(mut self) -> serde_json::Value {
        self.close();
        serde_json::Value::Object(
            self.totals
                .into_iter()
                .map(|(k, v)| (k, serde_json::json!((v * 10.0).round() / 10.0)))
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests;
