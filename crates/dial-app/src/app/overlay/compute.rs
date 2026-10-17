//! Compute monitor: tracks compute starts and releases while an overlay is open.

use crate::agent::Agent;
use crate::app::core::App;
use crate::app::discovery::shared;
use crate::learn::auto;
use crate::storage::store::Store;
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(in crate::app) struct ComputeMonitor {
    pub(in crate::app) snapshot: Arc<Mutex<Option<Value>>>,
    pub(in crate::app) message: Arc<Mutex<Option<(String, bool)>>>,
    pub(in crate::app) commands: std::sync::mpsc::SyncSender<Value>,
    pub(in crate::app) cancel: Arc<AtomicBool>,
}

impl ComputeMonitor {
    pub(in crate::app) fn new(app: &App) -> Self {
        let snapshot = Arc::new(Mutex::new(None));
        let message = Arc::new(Mutex::new(None));
        let cancel = Arc::new(AtomicBool::new(false));
        let (commands, rx) = std::sync::mpsc::sync_channel::<Value>(8);
        let output = snapshot.clone();
        let status = message.clone();
        let stop = cancel.clone();
        let layout = app.layout.clone();
        let config = app.config.clone();
        std::thread::spawn(move || {
            let mut next = Instant::now();
            while !stop.load(Ordering::SeqCst) {
                if let Ok(action) = rx.recv_timeout(Duration::from_millis(80)) {
                    let name = action["action"].as_str().unwrap_or("");
                    let result = (|| -> Result<String> {
                        match name {
                            "start_current" => {
                                ensure!(
                                    !crate::learn::compute::paused(&layout.user_dir),
                                    "Resume background work first"
                                );
                                ensure!(
                                    config.improve.enabled,
                                    "Automatic improvement is off for this model. Turn on Enabled in {} first.",
                                    crate::learn::improve::SETTINGS_PATH
                                );
                                auto::resume(&config.state_dir)?;
                                let store = Store::open(
                                    &config.state_dir,
                                    &config.model,
                                    &config.use_case,
                                    &config.harness,
                                )?;
                                store.set_meta("auto_enabled", "true")?;
                                auto::start_cycle(&layout, &config, &store)?
                                    .ensure_started(&config.state_dir)?;
                                Ok("Improvement started; coding keeps priority.".into())
                            }
                            "stop_now" => crate::learn::compute_controls::stop_legacy(
                                &layout.user_dir,
                                action["state"].as_str().context("select a job")?,
                            ),
                            "stop_process" => crate::learn::processes::stop(
                                &layout.user_dir,
                                action["pid"]
                                    .as_i64()
                                    .and_then(|pid| i32::try_from(pid).ok())
                                    .context("select a process")?,
                                action["identity"]
                                    .as_str()
                                    .context("missing process identity")?,
                            ),
                            _ => crate::learn::compute_controls::action(
                                &layout.user_dir,
                                action["state"].as_str(),
                                name,
                            ),
                        }
                    })();
                    *shared(&status) = Some(match result {
                        Ok(text) => (text, false),
                        Err(error) => (format!("{error:#}"), true),
                    });
                    next = Instant::now();
                }
                if Instant::now() >= next {
                    match crate::learn::compute_controls::snapshot(&layout.user_dir) {
                        Ok(value) => *shared(&output) = Some(value),
                        Err(error) => {
                            *shared(&status) =
                                Some((format!("Compute status unavailable: {error:#}"), true))
                        }
                    }
                    next = Instant::now() + Duration::from_millis(750);
                }
            }
        });
        Self {
            snapshot,
            message,
            commands,
            cancel,
        }
    }
}

impl Drop for ComputeMonitor {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}

/// The compute release a panel may run on a compute action; absent without an agent (welcome).
pub(in crate::app) fn overlay_compute_release<'a>(
    app: &'a App,
    agent: Option<&'a mut Agent<'_>>,
) -> Option<impl FnMut() + 'a> {
    agent.map(move |agent| {
        move || {
            app.release_idle_compute(agent);
        }
    })
}

/// Compute panel actions that start local training, which needs the session's compute lease and
/// unloaded models; stop, pause and inspection actions keep residency.
pub(in crate::app) fn compute_starts(payload: &str) -> bool {
    serde_json::from_str::<Value>(payload)
        .ok()
        .and_then(|action| {
            action["action"]
                .as_str()
                .map(|name| matches!(name, "start_current" | "start"))
        })
        .unwrap_or(false)
}
