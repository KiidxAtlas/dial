//! `/ratchet <flow> <subcommand>`: eval gate and hill-climb runner (see `agent::modes::ratchet`).
//!
//! Subcommands: `init`, `plan`, `split`, `approve <inputs|grader|plan>`, `status`, `climb`.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};

use crate::agent::modes::ratchet::{
    self, Direction, GoalOptions, Halt, InitOptions, PlanOptions, Proposer, ShellEvaluator, Stage,
    State,
};
use crate::agent::{Agent, NullUi, TurnOutcome};
use crate::app::{App, Shell, Step};

const USAGE: &str = "Usage:\n\
  /ratchet <flow> init --cases a,b --harness a,b --change a,b [--off-limits a,b]\n\
  /ratchet <flow> plan [--target m] [--direction higher|lower] [--hold a,b] [--directions m:lower,...] [--reps N] [--plateau N] [--rounds N] [--command <rest of line>]\n\
  /ratchet <flow> split --tags <file.json> [--test-fraction 0.4] [--seed N]\n\
  /ratchet <flow> approve <inputs|grader|plan>\n\
  /ratchet <flow> status\n\
  /ratchet <flow> climb";

pub(super) fn run(shell: &mut Shell, app: &App, agent: &mut Agent<'_>, args: &str) -> Result<Step> {
    let text = match dispatch(shell, app, agent, args) {
        Ok(Some(text)) => text,
        Ok(None) => return Ok(Step::Done),
        Err(error) => format!("{error:#}"),
    };
    shell.note("Ratchet", text);
    Ok(Step::Done)
}

fn dispatch(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    args: &str,
) -> Result<Option<String>> {
    let (flow, tail) = split_word(args.trim());
    let (subcommand, rest) = split_word(tail);
    if flow.is_empty() || subcommand.is_empty() {
        return Ok(Some(USAGE.into()));
    }
    let root = agent.workspace().root().to_path_buf();

    match subcommand {
        "init" => {
            let flags = parse_flags(rest)?;
            let state = ratchet::init(
                &root,
                flow,
                InitOptions {
                    cases: list(&flags, "cases"),
                    harness: list(&flags, "harness"),
                    change: list(&flags, "change"),
                    off_limits: list(&flags, "off-limits"),
                },
            )?;
            Ok(Some(format!(
                "Initialized flow {} (cases: {}; harness: {}; change: {})",
                state.flow,
                state.cases_paths.join(", "),
                state.harness_paths.join(", "),
                state.change_paths.join(", ")
            )))
        }
        "plan" => {
            let flags = parse_flags(rest)?;
            let mut state = ratchet::require(&root, flow)?;
            let goal = match flags.get("target") {
                Some(target) => Some(GoalOptions {
                    target: target.clone(),
                    direction: flags
                        .get("direction")
                        .map(|value| parse_direction(value))
                        .transpose()?,
                    hold: list(&flags, "hold"),
                    directions: parse_directions(
                        flags.get("directions").map(String::as_str).unwrap_or(""),
                    )?,
                }),
                None => {
                    for name in ["direction", "hold", "directions"] {
                        ensure!(!flags.contains_key(name), "--{name} needs --target");
                    }
                    None
                }
            };
            ratchet::plan(
                &mut state,
                PlanOptions {
                    goal,
                    reps: number(&flags, "reps")?,
                    plateau: number(&flags, "plateau")?,
                    rounds: number(&flags, "rounds")?,
                    command: command_of(rest),
                },
            )?;
            ratchet::save(&root, &state)?;
            Ok(Some(render_plan(&state)))
        }
        "split" => {
            let flags = parse_flags(rest)?;
            let tags_path = flags
                .get("tags")
                .context("split needs --tags <file.json>")?;
            let tags_file = match tags_path.strip_prefix("~/") {
                Some(relative) => std::path::PathBuf::from(
                    std::env::var_os("HOME").context("--tags ~/ needs HOME to be set")?,
                )
                .join(relative),
                None => root.join(tags_path),
            };
            let bytes = std::fs::read(&tags_file)
                .with_context(|| format!("reading tags file {tags_path}"))?;
            let tags: BTreeMap<String, String> = serde_json::from_slice(&bytes)
                .with_context(|| format!("{tags_path} must be a JSON object of case id to tag"))?;
            let fraction = match flags.get("test-fraction") {
                Some(value) => value
                    .parse::<f64>()
                    .with_context(|| format!("--test-fraction {value:?} is not a number"))?,
                None => 0.4,
            };
            let mut state = ratchet::require(&root, flow)?;
            ratchet::split(&mut state, &tags, fraction, number(&flags, "seed")?)?;
            ratchet::save(&root, &state)?;
            Ok(Some(format!(
                "Split {} cases: {} train, {} test (seed {})",
                state.train_ids.len() + state.test_ids.len(),
                state.train_ids.len(),
                state.test_ids.len(),
                state.split_seed.unwrap_or_default()
            )))
        }
        "approve" => {
            let stage_name = rest.trim();
            let stage = Stage::parse(stage_name).with_context(|| {
                format!("approve needs one of inputs, grader, plan; got {stage_name:?}")
            })?;
            let mut state = ratchet::require(&root, flow)?;
            let digest = ratchet::approve(&root, &mut state, stage)?;
            ratchet::save(&root, &state)?;
            Ok(Some(format!("Approved {} at {digest}", stage.name())))
        }
        "status" => {
            let state = ratchet::require(&root, flow)?;
            let approvals: Vec<String> = ratchet::approval_status(&root, &state)
                .into_iter()
                .map(|(stage, status)| format!("{}: {status}", stage.name()))
                .collect();
            Ok(Some(format!(
                "{}\nApprovals: {}\n\n{}",
                render_plan(&state),
                approvals.join(", "),
                ratchet::render_status_table(&state)
            )))
        }
        "climb" => {
            if !shell.wait_compute(app)? {
                return Ok(None);
            }
            let halt = ratchet::climb(
                &root,
                flow,
                &mut AgentProposer { agent },
                &mut ShellEvaluator,
            )?;
            let state = ratchet::require(&root, flow)?;
            Ok(Some(format!(
                "{}\n\n{}",
                describe_halt(&halt),
                ratchet::render_status_table(&state)
            )))
        }
        other => bail!("Unknown ratchet subcommand {other:?}\n{USAGE}"),
    }
}

/// Proposes candidates by running one agent turn that edits only the change paths.
struct AgentProposer<'a, 'b> {
    agent: &'a mut Agent<'b>,
}

impl Proposer for AgentProposer<'_, '_> {
    fn propose(&mut self, _root: &Path, state: &State, variant: &str) -> Result<Option<String>> {
        let prompt = candidate_prompt(state, variant);
        let report = self.agent.turn(&prompt, &mut NullUi)?;
        ensure!(
            matches!(report.outcome, TurnOutcome::Completed),
            "candidate turn for {variant} did not complete: {:?}",
            report.outcome
        );
        let line = report
            .text
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("");
        if line.is_empty() || line.eq_ignore_ascii_case("NONE") {
            return Ok(None);
        }
        Ok(Some(line.to_string()))
    }
}

fn candidate_prompt(state: &State, variant: &str) -> String {
    let mut prompt = format!(
        "Ratchet flow {} proposes {variant}. Make exactly one change to these paths: {}. Do not edit the cases ({}), the harness ({}), or off-limits paths ({}).\n",
        state.flow,
        state.change_paths.join(", "),
        state.cases_paths.join(", "),
        state.harness_paths.join(", "),
        state.off_limits.join(", "),
    );
    if let Some(goal) = &state.goal {
        let direction = match goal.direction {
            Direction::Higher => "higher is better",
            Direction::Lower => "lower is better",
        };
        prompt.push_str(&format!("Goal: improve {} ({direction}).\n", goal.target));
    }
    let recent: Vec<_> = state.rounds.iter().rev().take(3).collect();
    if !recent.is_empty() {
        prompt.push_str("Recent rounds, newest first:\n");
        for round in recent {
            prompt.push_str(&format!(
                "- {} {:?}: {} ({})\n",
                round.variant,
                round.decision,
                round.change,
                round.reasons.join("; ")
            ));
        }
    }
    prompt.push_str(
        "Reply with one line describing the change you made, or NONE if you have no candidate.",
    );
    prompt
}

fn describe_halt(halt: &Halt) -> String {
    match halt {
        Halt::Done => "Climb finished: stop condition reached.".into(),
        Halt::NoCandidate => "Climb stopped: the proposer returned no candidate.".into(),
        Halt::Rerun { variant } => format!(
            "Climb paused: {variant} needs another evaluator pass; run /ratchet <flow> climb to resume it."
        ),
    }
}

fn render_plan(state: &State) -> String {
    let mut lines = vec![format!("Flow {}", state.flow)];
    lines.push(format!("Cases: {}", state.cases_paths.join(", ")));
    lines.push(format!("Harness: {}", state.harness_paths.join(", ")));
    lines.push(format!("Change: {}", state.change_paths.join(", ")));
    if let Some(goal) = &state.goal {
        lines.push(format!(
            "Goal: {} ({:?}), hold: {}",
            goal.target,
            goal.direction,
            goal.hold.join(", ")
        ));
    }
    lines.push(format!(
        "Reps: {}",
        state
            .reps
            .map(|r| r.to_string())
            .unwrap_or_else(|| "unset".into())
    ));
    lines.push(format!(
        "Stop: plateau {}, rounds {}",
        state.stop.plateau,
        state
            .stop
            .rounds
            .map(|r| r.to_string())
            .unwrap_or_else(|| "unlimited".into())
    ));
    lines.push(format!(
        "Split: {} train, {} test",
        state.train_ids.len(),
        state.test_ids.len()
    ));
    lines.join("\n")
}

/// Splits off the first whitespace-delimited word; the remainder has leading whitespace removed.
fn split_word(text: &str) -> (&str, &str) {
    match text.find(char::is_whitespace) {
        Some(index) => (&text[..index], text[index..].trim_start()),
        None => (text, ""),
    }
}

/// Parses `--name value` pairs. `--command` is handled by `command_of` and skipped here.
fn parse_flags(rest: &str) -> Result<BTreeMap<String, String>> {
    let head = match rest.find("--command") {
        Some(index) => &rest[..index],
        None => rest,
    };
    let mut flags = BTreeMap::new();
    let mut words = head.split_whitespace();
    while let Some(word) = words.next() {
        let Some(name) = word.strip_prefix("--") else {
            bail!("Expected a --flag, got {word:?}");
        };
        let value = words
            .next()
            .with_context(|| format!("--{name} needs a value"))?;
        flags.insert(name.to_string(), value.to_string());
    }
    Ok(flags)
}

/// Everything after `--command` on the line, verbatim.
fn command_of(rest: &str) -> Option<String> {
    let index = rest.find("--command")?;
    let command = rest[index + "--command".len()..].trim();
    (!command.is_empty()).then(|| command.to_string())
}

fn list(flags: &BTreeMap<String, String>, name: &str) -> Vec<String> {
    flags
        .get(name)
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn number(flags: &BTreeMap<String, String>, name: &str) -> Result<Option<u32>> {
    flags
        .get(name)
        .map(|value| {
            value
                .parse::<u32>()
                .with_context(|| format!("--{name} {value:?} is not an integer"))
        })
        .transpose()
}

fn parse_direction(value: &str) -> Result<Direction> {
    match value {
        "higher" => Ok(Direction::Higher),
        "lower" => Ok(Direction::Lower),
        other => bail!("direction must be higher or lower, got {other:?}"),
    }
}

/// `--directions metric:lower,other:higher`
fn parse_directions(value: &str) -> Result<BTreeMap<String, Direction>> {
    let mut directions = BTreeMap::new();
    for item in value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
    {
        let (metric, direction) = item.split_once(':').with_context(|| {
            format!("--directions entry {item:?} must be metric:higher or metric:lower")
        })?;
        directions.insert(metric.to_string(), parse_direction(direction)?);
    }
    Ok(directions)
}
