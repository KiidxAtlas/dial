//! Hermetic case checks and self-play runs.

use super::*;

/// Marker error text for a hermetic run that used its whole step budget without answering.
pub(super) const BUDGET_MSG: &str = "agent exhausted the admitted step budget";

pub(super) fn seed_workspace(dir: &Path, files: &BTreeMap<String, String>) -> Result<()> {
    for (rel, content) in files {
        let p = Path::new(rel);
        ensure!(
            !rel.is_empty()
                && p.components()
                    .all(|c| matches!(c, std::path::Component::Normal(_))),
            "seeded file path must be relative"
        );
        let target = dir.join(p);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(target, content)?;
    }
    Ok(())
}

/// Run the agent hermetically on one task and return whether `check` passes on the final workspace
/// and answer. A stuck agent (step budget exhausted) fails the case; infrastructure errors propagate
/// so a broken model server aborts an evaluation instead of silently scoring zero.
pub fn check_case(
    runtime: &mut Runtime,
    config: &Config,
    version: &Version,
    prompt: &str,
    files: &BTreeMap<String, String>,
    check: &Check,
) -> Result<bool> {
    Ok(check_case_measured(runtime, config, version, prompt, files, check)?.passed)
}

/// The sealed verifier verdict plus observed workflow cost, without writing evaluation traces.
pub fn check_case_measured(
    runtime: &mut Runtime,
    config: &Config,
    version: &Version,
    prompt: &str,
    files: &BTreeMap<String, String>,
    check: &Check,
) -> Result<crate::learn::workflow::CaseMeasurement> {
    ensure!(
        !prompt.trim().is_empty() && prompt.len() <= 65536,
        "prompt must be 1..65536 bytes"
    );
    let workspace = tempfile::tempdir()?;
    seed_workspace(workspace.path(), files)?;
    let mut agent = Agent::hermetic(
        config,
        runtime.take_backend(config)?,
        version,
        workspace.path(),
        None,
        None,
    )?;
    let result = agent.turn(prompt, &mut NullUi);
    runtime.backend = agent.backend.take();
    let metrics = std::mem::take(agent.workflow_metrics.get_mut());
    let passed = match result {
        Ok(report) => {
            report.outcome == TurnOutcome::Completed && check.verify(workspace.path(), &report.text)
        }
        Err(error) if format!("{error:#}").contains(BUDGET_MSG) => false,
        Err(error) => return Err(error),
    };
    Ok(crate::learn::workflow::CaseMeasurement { passed, metrics })
}

/// Self-play: like `check_case`, but the trajectory is recorded (domain `selfplay:<family>`) with the
/// verifier's verdict as its outcome, so passing runs become training data and failures become signal.
pub fn selfplay_run(
    runtime: &mut Runtime,
    config: &Config,
    store: &Store,
    version: &Version,
    task: &crate::learn::curriculum::Task,
) -> Result<bool> {
    let crate::learn::curriculum::Task {
        family,
        prompt,
        files,
        check,
    } = task;
    let workspace = tempfile::tempdir()?;
    seed_workspace(workspace.path(), files)?;
    let mut agent = Agent::hermetic(
        config,
        runtime.take_backend(config)?,
        version,
        workspace.path(),
        Some(store),
        Some(format!("selfplay:{family}")),
    )?;
    let result = (|| -> Result<bool> {
        let mut input = prompt.to_owned();
        for attempt in 0..2 {
            let result = agent.turn(&input, &mut NullUi);
            let (trajectory, text, completed) = match result {
                Ok(report) => (
                    report.trajectory,
                    report.text,
                    report.outcome == TurnOutcome::Completed,
                ),
                Err(error) if format!("{error:#}").contains(BUDGET_MSG) => {
                    (agent.last_trajectory, String::new(), false)
                }
                Err(error) => return Err(error),
            };
            let passed = completed && check.verify(workspace.path(), &text);
            if let Some(id) = trajectory {
                store.outcome(&crate::config::types::Outcome {
                    trajectory: id,
                    score: if passed { 100.0 } else { 0.0 },
                    source: format!("verifier:{family}"),
                    reference: Some(format!("training attempt {}", attempt + 1)),
                })?;
            }
            if passed {
                return Ok(true);
            }
            if !completed || attempt == 1 {
                return Ok(false);
            }
            crate::learn::auto::check_stop(config)?;
            // This feedback is confined to training cases. Gate cases are never repaired or logged.
            input = "The independent training-task verifier rejected the result. Revisit the original requirements, inspect the smallest relevant implementation, and correct the behavior. Preserve tests and requirements. Change your approach instead of repeating the failed attempt, and run relevant checks before finishing.".into();
        }
        Ok(false)
    })();
    runtime.backend = agent.backend.take();
    result
}

/// Commands whose exit status is a meaningful external verdict on the work (tests, builds, linters).
pub(super) fn is_check_command(command: &str) -> bool {
    const MARKERS: &[&str] = &[
        "pytest",
        "cargo test",
        "cargo check",
        "cargo build",
        "cargo clippy",
        "npm test",
        "npm run test",
        "npm run build",
        "npm run lint",
        "yarn test",
        "pnpm test",
        "go test",
        "go build",
        "go vet",
        "make test",
        "make check",
        "make build",
        "tsc",
        "mvn test",
        "gradle test",
        "ruff",
        "flake8",
        "mypy",
        "eslint",
        "unittest",
        "swift test",
        "dotnet test",
        "bun test",
        "deno test",
        "rspec",
        "phpunit",
        "ctest",
    ];
    let c = command.to_lowercase();
    MARKERS.iter().any(|m| c.contains(m))
}
