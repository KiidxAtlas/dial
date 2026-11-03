//! `dial weights`: weight inspection and evaluation-gated weight edits of the local MLX model.
use crate::args::{Cli, WeightsCommand};
use crate::overlay::overlay;
use anyhow::{Context, Result};
use dial::config::settings_store::{self, Scope};
use dial::config::{paths::Layout, types::Config};
use dial::weights::{self, Operation};
use serde_json::{Value, json};
use std::path::Path;

pub(crate) fn run(cli: &Cli, layout: &Layout, action: &WeightsCommand) -> Result<()> {
    // Apply `--provider` like a session does, so `--provider mlx --model X` selects the local model.
    let config = dial::cli::model_flags::apply(
        Config::load(layout, Some(overlay(cli)?))?,
        &cli.model_flags,
    )?;
    let cancel = dial::platform::interrupt::install_sigint();
    let (op, input, adapter, json) = match action {
        WeightsCommand::Status { json } => {
            let value = weights::status(&config);
            return print(&value, *json, weights::render_status);
        }
        WeightsCommand::Open { port } => return open(&config, *port),
        WeightsCommand::Experiment { name, budget, json } => {
            let value = match budget {
                Some(budget) => weights::create_experiment(&config, name, *budget)?,
                None => weights::experiment(&config, name)?,
            };
            return print(&value, *json, weights::render);
        }
        WeightsCommand::Evaluate {
            edit,
            cases,
            max_tokens,
            json,
        } => {
            let input = json!({"edit": edit, "cases": read(cases)?, "max_tokens": max_tokens});
            let value = weights::evaluate(&config, Some(layout), &input, &cancel)?;
            return print(&value, *json, weights::render);
        }
        WeightsCommand::Promote { edit } => {
            return switch(layout, weights::promotion(&config, edit)?);
        }
        WeightsCommand::Revert => return switch(layout, weights::reversion(&config)?),
        WeightsCommand::Map { json } => (Operation::Map, json!({}), None, *json),
        WeightsCommand::Stats { prefix, json } => {
            (Operation::Stats, json!({ "prefix": prefix }), None, *json)
        }
        WeightsCommand::Activations {
            pairs,
            adapter,
            json,
        } => (
            Operation::Activations,
            json!({ "pairs": read(pairs)? }),
            adapter.as_deref(),
            *json,
        ),
        WeightsCommand::Trace {
            cases,
            heads,
            layers,
            adapter,
            json,
        } => {
            let mut input = json!({ "cases": read(cases)? });
            if let Some(layer) = heads {
                input["mode"] = json!("heads");
                input["layer"] = json!(layer);
            } else if !layers.is_empty() {
                input["layers"] = json!(layers);
            }
            (Operation::Trace, input, adapter.as_deref(), *json)
        }
        WeightsCommand::Generate {
            experiment,
            cases,
            max_tokens,
            tail,
            adapter,
            json,
        } => (
            Operation::Generate,
            json!({"experiment": experiment, "cases": read(cases)?, "max_tokens": max_tokens, "tail": tail}),
            adapter.as_deref(),
            *json,
        ),
        WeightsCommand::Direction {
            name,
            experiment,
            positive,
            negative,
            site,
            json,
        } => (
            Operation::Direction,
            json!({"experiment": experiment, "name": name, "positive": read(positive)?,
                   "negative": read(negative)?, "site": site}),
            None,
            *json,
        ),
        WeightsCommand::Edit {
            name,
            experiment,
            direction,
            layers,
            columns,
            alpha,
            json,
        } => (
            Operation::Edit,
            json!({"experiment": experiment, "name": name, "direction": direction,
                   "layers": layers, "columns": columns, "alpha": alpha}),
            None,
            *json,
        ),
    };
    let value = weights::run(&config, Some(layout), op, &input, adapter, &cancel)?;
    print(&value, json, weights::render)
}

fn print(value: &Value, json: bool, render: fn(&Value) -> String) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        println!("{}", render(value));
    }
    Ok(())
}

/// Save a model switch to this project's settings.
fn switch(layout: &Layout, switch: weights::ModelSwitch) -> Result<()> {
    settings_store::save_model_config(layout, Scope::Project, &switch.changes, "main")?;
    println!("{}", switch.message);
    Ok(())
}

fn read(path: &Path) -> Result<Value> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("{} is not valid JSON", path.display()))
}

fn open(config: &Config, port: u16) -> Result<()> {
    let listener = dial::stats::server::bind_loopback(port)?;
    let url = format!("http://{}/", listener.local_addr()?);
    eprintln!("Dial weights dashboard: {url} (Ctrl-C to stop)");
    dial::llm::oauth::util::open_browser(&url);
    weights::dashboard::serve_listener(listener, config.clone())
}
