//! The `security_scan` tool: every operation the `/security`
//! command offers, as one model-callable tool.

use std::sync::Arc;

use serde_json::{Value, json};

use super::ops::{self, Env, OPERATIONS};
use super::review::Reviewer;
use crate::config::types::Config;
use crate::runtime::schema::tool_schema;
use crate::runtime::tool_output::ToolOutput;
use crate::tools::ext::{Tool, ToolContext};

pub const NAME: &str = "security_scan";

const DESCRIPTION: &str = "Plan, import, inspect, compare, and manage security scan results for the \
current Git repository, and drive Codex Security cloud configurations. Read-only operations: \
cloud_configs, cloud_status, scans, show, export, compare, lineage. Mutating operations: preflight, \
cloud_create, cloud_pull, import, disposition. Scan execution (scan, start) and validate run the \
model-driven security coordinator and reviewers; status and cancel manage executing scans.";

pub struct SecurityScanTool;

fn read_only(op: &str) -> bool {
    matches!(
        op,
        "status"
            | "cloud_configs"
            | "cloud_status"
            | "scans"
            | "show"
            | "export"
            | "compare"
            | "lineage"
    )
}

impl Tool for SecurityScanTool {
    fn name(&self) -> &str {
        NAME
    }

    fn schema(&self) -> Value {
        let properties = json!({
            "operation": { "type": "string", "enum": OPERATIONS, "description": "Operation to run." },
            "account": { "type": "string", "description": "ChatGPT account client id to act as reviewer. Required only when several accounts are signed in." },
            "target": { "type": "string", "enum": ["repository", "scoped_path", "ref_diff"], "description": "Scan target for preflight (default repository)." },
            "include": { "type": "array", "items": { "type": "string" }, "description": "Repository-relative include paths." },
            "exclude": { "type": "array", "items": { "type": "string" }, "description": "Repository-relative exclude paths." },
            "base": { "type": "string", "description": "Base revision for ref_diff targets." },
            "head": { "type": "string", "description": "Head revision for ref_diff targets." },
            "knowledgeBases": { "type": "array", "items": { "type": "string" }, "description": "Knowledge base paths recorded in the plan." },
            "output": { "type": "string", "description": "Output directory for the plan (default: user data directory)." },
            "archiveExisting": { "type": "boolean", "description": "Archive an existing output directory when planning." },
            "scan": { "type": "string", "description": "Scan id (secscan_...)." },
            "plan": { "type": "string", "description": "Saved plan id (secplan_...) to execute with scan instead of creating a new plan." },
            "finding": { "type": "string", "description": "Finding id (secf_...)." },
            "status": { "type": "string", "enum": ["open", "false_positive", "accepted_risk", "fixed", "wont_fix"], "description": "Disposition for disposition." },
            "before": { "type": "string", "description": "Earlier scan id for compare or lineage." },
            "after": { "type": "string", "description": "Later scan id for compare or lineage." },
            "path": { "type": "string", "description": "SARIF file or Codex Security bundle directory for import, relative to the project root." },
            "configuration": { "type": "string", "description": "Codex Security cloud configuration id." },
            "repositoryId": { "type": "string", "description": "Codex Security repository id for cloud_create." },
            "repositoryUrl": { "type": "string", "description": "Repository URL for cloud_create (default: origin remote)." },
            "environmentId": { "type": "string", "description": "Codex Security environment id for cloud_create." },
            "lookback": { "type": "string", "description": "Lookback in days (positive integer) or `all`." },
            "attackConcerns": { "type": "string", "description": "Maintainer attack concerns for cloud_create." },
            "focusAreas": { "type": "string", "description": "Maintainer focus areas for cloud_create." },
            "additionalContext": { "type": "string", "description": "Maintainer additional context for cloud_create." },
        });
        tool_schema(NAME, DESCRIPTION, properties, &["operation"])
    }

    fn needs_approval(&self, args: &Value) -> bool {
        match args.get("operation").and_then(Value::as_str) {
            Some(op) => !read_only(op),
            None => true,
        }
    }

    fn call(&self, ctx: &ToolContext<'_>, args: &Value) -> ToolOutput {
        run_with(ctx.config, args, None)
    }
}

/// Runs one `security_scan` operation. `runner` is the reviewer that executes model-driven work
/// (scan, start, validate). Without one, those operations explain that no reviewer is available.
pub fn run_with(config: &Config, args: &Value, runner: Option<&mut dyn Reviewer>) -> ToolOutput {
    let Some(op) = args.get("operation").and_then(Value::as_str) else {
        return ToolOutput::err("operation is required");
    };
    let env = Env::from_config(config);
    match ops::run(&env, op, args, runner) {
        Ok(text) => ToolOutput {
            execution: Default::default(),
            text,
            is_error: false,
            data: None,
        },
        Err(error) => ToolOutput::err(format!("{error:#}")),
    }
}

/// Tool instance for registration.
pub fn tool() -> Arc<dyn Tool> {
    Arc::new(SecurityScanTool)
}
