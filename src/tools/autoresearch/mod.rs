//! Model-facing controls for the current workspace's branch-scoped experiment.
pub mod experiment;

use super::{
    ToolOutput,
    ext::{Tool, ToolContext},
    tool_schema,
};
use crate::config::types::Config;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};

pub fn tools(config: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(AutoresearchTool {
        root: config.project_root.clone(),
        state: config.state_dir.join("autoresearch"),
    })]
}
struct AutoresearchTool {
    root: PathBuf,
    state: PathBuf,
}
impl Tool for AutoresearchTool {
    fn name(&self) -> &str {
        "autoresearch"
    }
    fn schema(&self) -> Value {
        tool_schema(
            "autoresearch",
            "Manage a benchmark experiment for this workspace. Actions: init (name, benchmark, metric, maximize), run, log, notes (text), status. Benchmarks run in the strict sandbox.",
            json!({"action":{"type":"string","enum":["init","run","log","notes","status"]},"name":{"type":"string"},"benchmark":{"type":"string"},"metric":{"type":"string"},"maximize":{"type":"boolean"},"text":{"type":"string"}}),
            &["action"],
        )
    }
    fn needs_approval(&self, args: &Value) -> bool {
        matches!(args["action"].as_str(), Some("init" | "run" | "notes"))
    }
    fn call(&self, _ctx: &ToolContext, args: &Value) -> ToolOutput {
        let result = (|| -> anyhow::Result<String> {
            match args["action"].as_str().unwrap_or("status") {
                "init" => {
                    let e = experiment::init(
                        &self.root,
                        &self.state,
                        args["name"].as_str().unwrap_or(""),
                        args["benchmark"].as_str().unwrap_or(""),
                        args["metric"].as_str().unwrap_or(""),
                        args["maximize"].as_bool().unwrap_or(true),
                    )?;
                    Ok(format!("Initialized {} on {}", e.name, e.branch))
                }
                "run" => {
                    let r = experiment::run(&self.root, &self.state)?;
                    Ok(format!(
                        "metric={} {}\n{}",
                        r.metric,
                        if r.kept { "best" } else { "not best" },
                        r.output
                    ))
                }
                "log" => Ok(experiment::log(&self.state)?
                    .into_iter()
                    .map(|r| {
                        format!(
                            "#{} {} {} {}",
                            r.id,
                            r.metric,
                            if r.kept { "best" } else { "" },
                            r.commit
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")),
                "notes" => {
                    let Some(t) = args["text"].as_str() else {
                        anyhow::bail!("notes requires text");
                    };
                    experiment::update_notes(&self.state, t)?;
                    Ok("Notes updated".into())
                }
                "status" => Ok(experiment::current(&self.state)?
                    .map(|e| format!("{} · {} · {}", e.name, e.metric, e.branch))
                    .unwrap_or_else(|| "No experiment initialized".into())),
                action => anyhow::bail!("unknown autoresearch action: {action}"),
            }
        })();
        match result {
            Ok(text) => ToolOutput::structured(json!({"text":text})),
            Err(e) => ToolOutput::err(format!("{e:#}")),
        }
    }
}
