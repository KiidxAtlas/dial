//! Reliability and adaptive-policy inspection commands.
use crate::args::{Cli, ReliabilityCommand};
use crate::overlay::overlay;
use anyhow::Result;
use dial::config::{paths::Layout, types::Config};

pub(crate) fn reliability(cli: &Cli, layout: &Layout, action: &ReliabilityCommand) -> Result<()> {
    let config = Config::load(layout, Some(overlay(cli)?))?;
    let (value, json) = match action {
        ReliabilityCommand::Status { json } => (
            dial::reliability::report(&config, &serde_json::json!({})),
            *json,
        ),
        ReliabilityCommand::Check { json } => {
            let checked = dial::reliability::check(&config)?;
            let mut value = dial::reliability::report(&config, &serde_json::json!({}));
            value["measured"] = checked;
            (value, *json)
        }
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("{}", dial::reliability::render(&value));
    }
    Ok(())
}

pub(crate) fn policy(
    cli: &Cli,
    layout: &Layout,
    prompt: &Option<String>,
    json: bool,
) -> Result<()> {
    let config = Config::load(layout, Some(overlay(cli)?))?;
    let value = if let Some(prompt) = prompt {
        let task = dial::learn::task_policy::Policy::for_prompt(
            prompt,
            config.reasoning_guard.max_pre_action_secs,
        );
        let phase = dial::adaptive::State::begin(task.kind, prompt);
        let h = config.effective_harness(&config.harness)?;
        let selection = dial::adaptive::select(
            &config,
            &phase,
            dial::adaptive::Request {
                kind: task.kind,
                temperature: h.temperature,
                tokens: h.max_tokens,
                pre_action_secs: task.initial_reasoning_secs,
                base_profile: config.active_model_profile(),
                recovering_output: false,
                hermetic: false,
            },
        );
        let mut control = serde_json::json!({"adaptive_policy":{"selection":selection,"result":{"state":"preview only; configured baseline, no model request"}}});
        control["adaptive_history"] = serde_json::json!([]);
        dial::adaptive::report(&config, &control)
    } else {
        dial::adaptive::report(&config, &serde_json::json!({}))
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("{}", dial::adaptive::render(&value));
    }
    Ok(())
}
