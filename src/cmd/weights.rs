//! `dial weights`: weight inspection and evaluation-gated weight edits of the local MLX model.
use crate::args::{Cli, WeightsCommand};
use crate::overlay::overlay;
use anyhow::{Context, Result};
use dial::config::settings_store::{self, Scope};
use dial::config::{paths::Layout, types::Config};
use dial::storage::session::Session;
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
        WeightsCommand::History { op, json } => {
            let rows = weights::history(&config, op.as_deref());
            return print(&json!(rows), *json, weights::render_history);
        }
        WeightsCommand::Show { id, json } => {
            return print(&weights::show_run(&config, id)?, *json, weights::render);
        }
        WeightsCommand::Compare {
            before,
            after,
            json,
        } => {
            let value = weights::compare_analyses(
                &weights::show_run(&config, before)?,
                &weights::show_run(&config, after)?,
            )?;
            return print(&value, *json, weights::render);
        }
        WeightsCommand::Prune { keep } => {
            let removed = weights::prune(&config, *keep)?;
            println!("Removed {removed} kept weights runs; the newest {keep} remain.");
            return Ok(());
        }
        WeightsCommand::Combine {
            name,
            edits,
            allow_noisy,
            json,
        } => {
            let input = json!({"name": name, "edits": edits, "allow_noisy": allow_noisy});
            let value = weights::combine(&config, Some(layout), &input, &cancel)?;
            return print(&value, *json, weights::render);
        }
        WeightsCommand::Cases {
            session,
            limit,
            context,
            output,
        } => return cases(layout, session, *limit, *context, output.as_deref()),
        WeightsCommand::Next => {
            for step in weights::next_steps(&weights::status(&config)) {
                println!("{step}");
            }
            return Ok(());
        }
        WeightsCommand::Map { json } => (Operation::Map, json!({}), None, *json),
        WeightsCommand::Analyze { json } => (Operation::Analyze, json!({}), None, *json),
        WeightsCommand::Improve { .. } => {
            anyhow::bail!("`weights improve` starts a session and is handled before this point")
        }
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
        WeightsCommand::Attribute {
            experiment,
            cases,
            wanted,
            neutral,
            focus,
            top,
            json,
        } => (
            Operation::Attribute,
            json!({"experiment": experiment, "cases": read(cases)?,
                   "wanted": wanted.as_deref().map(read).transpose()?,
                   "neutral": neutral.as_deref().map(read).transpose()?,
                   "focus": focus, "top": top}),
            None,
            *json,
        ),
        WeightsCommand::Try {
            experiment,
            cases,
            neutral,
            doses,
            direction,
            layers,
            changes,
            columns,
            alpha,
            allow_noisy,
            json,
        } => {
            let mut input = json!({"experiment": experiment, "cases": read(cases)?,
                "neutral": neutral.as_deref().map(read).transpose()?,
                "doses": (!doses.is_empty()).then_some(doses), "allow_noisy": allow_noisy});
            match changes {
                Some(changes) => {
                    input["method"] = json!("scale");
                    input["changes"] = read(changes)?;
                }
                None => {
                    input["direction"] = json!(direction);
                    input["layers"] = json!(layers);
                    input["columns"] = json!(columns);
                    input["alpha"] = json!(alpha);
                }
            }
            (Operation::Try, input, None, *json)
        }
        WeightsCommand::Edit {
            name,
            experiment,
            direction,
            layers,
            bundle,
            data,
            strength,
            changes,
            columns,
            alpha,
            json,
        } => (
            Operation::Edit,
            match (changes, bundle) {
                (Some(changes), _) => json!({"experiment": experiment, "name": name,
                    "method": "scale", "changes": read(changes)?}),
                (None, Some(bundle)) => json!({"experiment": experiment, "name": name,
                    "method": "steering", "bundle": absolute(bundle)?,
                    "data": absolute(data.as_deref().context("--data is required with --bundle")?)?,
                    "strength": strength}),
                (None, None) => {
                    json!({"experiment": experiment, "name": name, "direction": direction,
                    "layers": layers, "columns": columns, "alpha": alpha})
                }
            },
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
    match switch.role {
        Some(role) => {
            settings_store::save_model_config(layout, Scope::Project, &switch.changes, role)?
        }
        None => settings_store::save_config(layout, Scope::Project, &switch.changes)?,
    };
    println!("{}", switch.message);
    Ok(())
}

fn read(path: &Path) -> Result<Value> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("{} is not valid JSON", path.display()))
}

fn absolute(path: &Path) -> Result<String> {
    let path =
        std::fs::canonicalize(path).with_context(|| format!("{} not found", path.display()))?;
    Ok(path.to_string_lossy().into_owned())
}

/// `dial weights cases`: generation cases from a past session's user turns.
fn cases(
    layout: &Layout,
    session: &str,
    limit: usize,
    context: usize,
    output: Option<&Path>,
) -> Result<()> {
    anyhow::ensure!((1..=32).contains(&limit), "--limit must be 1-32");
    let found = Session::find_prefix(layout, session)?;
    let info = match found.as_slice() {
        [info] => info,
        [] => anyhow::bail!("no session matches {session}"),
        _ => anyhow::bail!(
            "{session} matches {} sessions; give more of the id",
            found.len()
        ),
    };
    let messages = Session::read(&info.path)?.messages();
    let cases = weights::cases_from_messages(&messages, limit, context);
    anyhow::ensure!(
        !cases.is_empty(),
        "session {} has no user turns with text",
        info.id
    );
    let text = serde_json::to_string_pretty(&cases)?;
    match output {
        Some(path) => {
            std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
            eprintln!(
                "Wrote {} cases to {}. Add `required`/`forbidden` checks where an answer must contain or avoid text.",
                cases.len(),
                path.display()
            );
        }
        None => println!("{text}"),
    }
    Ok(())
}

fn open(config: &Config, port: u16) -> Result<()> {
    let listener = dial::stats::server::bind_loopback(port)?;
    let url = format!("http://{}/", listener.local_addr()?);
    eprintln!("Dial weights dashboard: {url} (Ctrl-C to stop)");
    dial::llm::oauth::util::open_browser(&url);
    weights::dashboard::serve_listener(listener, config.clone())
}
