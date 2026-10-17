//! Text and view models for the improve overlay and its status lines.

use crate::app::core::App;
use crate::app::display::{cycle_steps, state_words, version_line};
use crate::app::learning::lora_state;
use crate::app::shell::teach::TEACH_OFF;
use crate::learn::auto;
use crate::storage::store::Store;
use serde_json::Value;

/// `/improve jobs`: project-wide compute status. It reads job files only, so it runs during a turn.
pub(super) fn improve_jobs_text(app: &App) -> String {
    auto::jobs_summary(&app.layout).unwrap_or_else(|error| format!("{error:#}"))
}

/// `/improve stop <id>`. A bare stop lists the running jobs and their ids instead of guessing one.
pub(super) fn improve_stop_text(app: &App, id: &str) -> String {
    let id = id.trim();
    if id.is_empty() {
        let running: Vec<String> = auto::jobs(&app.layout)
            .ok()
            .and_then(|jobs| jobs["project_jobs"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter(|job| job["control_busy"] == true)
            .filter_map(|job| job["state_id"].as_str().map(str::to_owned))
            .collect();
        return if running.is_empty() {
            "usage: /improve stop <state id>. No improvement job is running in this project.".into()
        } else {
            format!(
                "usage: /improve stop <state id>. Running jobs: {}",
                running.join(", ")
            )
        };
    }
    match auto::request_stop(&app.layout, id) {
        Ok(value) => value["message"]
            .as_str()
            .unwrap_or("stop requested")
            .to_owned(),
        Err(error) => format!("{error:#}"),
    }
}

/// The `/improve` panel's snapshot: the facts of `/improve status` as plain values. It reads the
/// store and job files, so the controller calls it on open and while a job runs, never from draw.
pub(super) fn improve_view(app: &App) -> crate::ui::overlay::improve::View {
    use crate::ui::overlay::improve::{JobRow, VersionRow, View};
    let cached = Store::open_read_only(&app.config.state_dir).ok();
    let needed = crate::learn::improve::min_examples(&app.config);
    let Some(store) = app.store.as_ref().or(cached.as_ref()) else {
        return View {
            enabled: app.config.improve.enabled,
            readiness: "the learning store is unavailable".into(),
            state: "unavailable".into(),
            needed,
            ..View::default()
        };
    };
    let status = store
        .get_meta("auto_status")
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or(Value::Null);
    let state = status["state"].as_str().unwrap_or("");
    let mut progress = Vec::new();
    if matches!(state, "queued" | "running") {
        let cycle = store
            .get_meta("cycle_progress")
            .ok()
            .flatten()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .unwrap_or(Value::Null);
        progress.push(format!(
            "Stage: {}",
            cycle["stage"].as_str().unwrap_or("starting")
        ));
        if let Some(started) = cycle["started"].as_u64() {
            progress.push(format!(
                "Elapsed: {}",
                auto::duration(crate::learn::improve::now().saturating_sub(started))
            ));
        }
        if let (Some(iteration), Some(total)) = (
            cycle["training"]["iteration"].as_u64(),
            cycle["training"]["total"].as_u64(),
        ) {
            progress.push(format!(
                "Training: {} {iteration}/{total}",
                cycle["training"]["phase"].as_str().unwrap_or("step")
            ));
        }
    }
    let last_cycle = store
        .get_meta("last_cycle_report")
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .map(|report| cycle_steps(&report))
        .unwrap_or_default();
    let active = store.active().ok();
    let live = active.as_ref().map(|version| version.id);
    let previous = active.as_ref().and_then(|version| version.parent);
    let mut jobs: Vec<JobRow> = auto::jobs(&app.layout)
        .ok()
        .and_then(|jobs| jobs["project_jobs"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter(|job| job.get("error").is_none())
        .map(|job| {
            let active = job["control_busy"] == true;
            let stage = job["progress"]["stage"].as_str().unwrap_or(if active {
                "running"
            } else {
                "idle"
            });
            JobRow {
                id: job["state_id"].as_str().unwrap_or("").to_owned(),
                label: format!(
                    "{} · {stage}{}",
                    job["model"].as_str().unwrap_or("unknown model"),
                    if job["stop_requested"] == true {
                        " · stop requested"
                    } else {
                        ""
                    }
                ),
                active,
                stopping: job["stop_requested"] == true,
            }
        })
        .collect();
    jobs.sort_by_key(|job| !job.active);
    let versions = store
        .versions()
        .unwrap_or_default()
        .iter()
        .rev()
        .map(|version| VersionRow {
            id: version.id,
            text: version_line(version, None),
            live: live == Some(version.id),
            previous: previous == Some(version.id),
        })
        .collect();
    View {
        enabled: app.config.improve.enabled,
        readiness: auto::readiness(&app.config, store)
            .unwrap_or_else(|error| format!("unavailable ({error:#})")),
        state: state_words(state),
        live_version: live,
        approved: store.verified_live_turns().unwrap_or(0),
        needed,
        progress,
        last_cycle,
        jobs,
        versions,
    }
}

pub(super) fn improve_summary(app: &App) -> String {
    let cached = Store::open_read_only(&app.config.state_dir).ok();
    let Some(store) = app.store.as_ref().or(cached.as_ref()) else {
        return format!(
            "{TEACH_OFF}\nWithout it, coding sessions are not recorded; `dial improve cycle` still runs an explicit evaluation."
        );
    };
    let version = store
        .active()
        .map(|active| format!("v{}", active.id))
        .unwrap_or_else(|error| format!("unavailable ({error:#})"));
    let state = store
        .get_meta("auto_status")
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| {
            value
                .get("state")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "idle".into());
    let enabled = auto::enabled(&app.config, store).unwrap_or(false);
    let readiness = auto::readiness(&app.config, store)
        .unwrap_or_else(|error| format!("schedule unavailable: {error:#}"));
    let state_text = state_words(&state);
    let mut summary = format!(
        "Selected model: {} · version {version} · {state_text} · next cycle: {}\nlearning records stay in {}",
        if enabled {
            "automatic improvement enabled"
        } else {
            "automatic improvement paused"
        },
        readiness,
        app.config.state_dir.display()
    );
    match auto::remaining(store, "last_training_attempt", app.config.training.interval_secs, crate::learn::improve::now()) {
        Ok(0) => summary.push_str("\nNext LoRA attempt: cooldown elapsed; training still requires verified data and a passing evaluation."),
        Ok(wait) => summary.push_str(&format!("\nNext LoRA attempt: cooldown {} remaining.", auto::duration(wait))),
        Err(error) => summary.push_str(&format!("\nLoRA cooldown unavailable: {error:#}")),
    }
    summary.push_str(&format!("\nTraining requires at least {} verified examples and a validation split. Cooldowns are eligibility delays, not training-duration estimates.", crate::learn::improve::min_examples(&app.config)));
    if app.store.is_none() {
        summary.push_str(&format!("\n{TEACH_OFF}"));
    }
    match store.verified_live_turns() {
        Ok(n) if app.store.is_some() => summary.push_str(&format!(
            "\nTeaching: {n} approved turns recorded. A turn is approved when you commit its changes, press Alt+Up, or its behavioral checks pass; /undo or Alt+Down rejects it."
        )),
        Ok(n) => summary.push_str(&format!("\nSession learning: {n} verified live turns recorded earlier.")),
        Err(error) => summary.push_str(&format!("\nSession learning unavailable: {error:#}")),
    }
    if state == "running"
        && let Ok(Some(raw)) = store.get_meta("cycle_progress")
        && let Ok(value) = serde_json::from_str::<Value>(&raw)
    {
        let elapsed = crate::learn::improve::now().saturating_sub(
            value["started"]
                .as_u64()
                .unwrap_or(crate::learn::improve::now()),
        );
        summary.push_str(&format!(
            "\nCurrent stage: {}\nCycle elapsed: {}. Completion ETA is not yet available.",
            value["stage"].as_str().unwrap_or("unknown"),
            auto::duration(elapsed)
        ));
        if let Some(peak) = value["training"]["peak_memory_gb"].as_f64() {
            summary.push_str(&format!(
                "\nLast training report: MLX peak allocated memory {peak:.2} GB."
            ));
        }
    }
    summary.push_str(&format!(
        "\n{}",
        lora_state(&app.config, Some(store), store.active().ok().as_ref()).label()
    ));
    if let Ok(Some(raw)) = store.get_meta("serving_capabilities")
        && let Ok(caps) = serde_json::from_str::<Value>(&raw)
        && crate::learn::worker::current_lora_probe(&caps)
        && caps["lora"] == false
    {
        summary.push_str(&format!("\nLoRA unavailable in Dial's current trainer: {}. This cycle can still evaluate harness improvements.", caps["reason"].as_str().unwrap_or("the model capability probe rejected adapter training")));
    }
    match store.latest_lora_admission() {
        Ok(Some((report, activated, decision))) => {
            summary.push_str(&format!(
                "\nLatest LoRA evaluation: v{} against v{} — {} ({decision})\n{}",
                report.candidate,
                report.baseline,
                if activated {
                    "accepted"
                } else {
                    "not activated"
                },
                report.reason
            ));
            for score in report.scores {
                summary.push_str(&format!(
                    "\n{}: baseline {:.3} → candidate {:.3} ({} cases)",
                    score.name, score.baseline, score.candidate, score.cases
                ));
            }
            summary.push_str("\nGate scores measure the evaluated tasks; adapter use alone does not prove improvement.");
        }
        Ok(None) => summary.push_str("\nNo LoRA evaluation recorded yet."),
        Err(error) => summary.push_str(&format!("\nLoRA evaluation status unavailable: {error:#}")),
    }
    if let Ok(Some(raw)) = store.get_meta("last_cycle_report")
        && let Ok(report) = serde_json::from_str::<Value>(&raw)
        && let Some(steps) = report["steps"].as_array()
    {
        for step in steps {
            if step["step"] == "lora_skipped" {
                summary.push_str(&format!(
                    "\nLast training skip: {}",
                    step["data"].as_str().unwrap_or("unknown")
                ));
            }
        }
    }
    if let Ok(tasks) = crate::learn::repository::load(&app.config)
        && !tasks.is_empty()
    {
        summary.push_str(&format!(
            "\nRepository exercises: {} training, {} sealed evaluation.",
            tasks.iter().filter(|t| t.spec.split == "train").count(),
            tasks
                .iter()
                .filter(|t| t.spec.split == "evaluation")
                .count()
        ));
    }
    if let Ok(profiles) = crate::learn::profiles::summary(store) {
        summary.push_str(&format!("\n{profiles}"));
    }
    match auto::jobs_summary(&app.layout) {
        Ok(jobs) => summary.push_str(&format!("\n{jobs}")),
        Err(error) => summary.push_str(&format!("\nShared compute status unavailable: {error:#}")),
    }
    summary.push_str("\nShared compute includes other models and sessions. Use /improve jobs for project-wide status; /improve stop <state_id> requests a safe stop and pauses that model.");
    summary
}
