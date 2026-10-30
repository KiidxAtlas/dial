//! Human-approved live inspection budgets shared by supported producers, not an execution grant.
use crate::config::types::Config;
use crate::tools::integrations::ext::{Tool, ToolContext};
use crate::tools::{ToolOutput, Workspace};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

/// Conservative reservations: failed attempts are charged and never silently refunded.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Spend {
    pub images: u64,
    pub pages: u64,
    pub frames: u64,
    pub audio_ms: u64,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Limits {
    images: u64,
    pages: u64,
    frames: u64,
    audio_ms: u64,
    operations: u64,
}
#[derive(Debug, Clone, Serialize)]
struct Budget {
    limits: Limits,
    used: Spend,
    operations: u64,
    question: String,
}
static BUDGETS: OnceLock<Mutex<BTreeMap<PathBuf, Budget>>> = OnceLock::new();
fn budgets() -> &'static Mutex<BTreeMap<PathBuf, Budget>> {
    BUDGETS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Charge immediately before supported inspection work, including direct producer calls.
pub(crate) fn reserve(ctx: &ToolContext, spend: Spend) -> Result<()> {
    reserve_workspace(ctx.workspace, spend)
}
pub(crate) fn reserve_workspace(ws: &Workspace, spend: Spend) -> Result<()> {
    let mut all = budgets().lock().expect("perception budgets lock poisoned");
    let Some(budget) = all.get_mut(ws.root()) else {
        return Ok(());
    };
    let add = |used: u64, amount: u64, limit: u64| -> Result<u64> {
        let next = used
            .checked_add(amount)
            .context("inspection budget arithmetic overflow")?;
        ensure!(
            next <= limit,
            "inspection budget exhausted; stop and ask the user before opening another budget"
        );
        Ok(next)
    };
    // Validate every dimension before publishing any reservation; concurrent calls serialize here.
    let next = Spend {
        images: add(budget.used.images, spend.images, budget.limits.images)?,
        pages: add(budget.used.pages, spend.pages, budget.limits.pages)?,
        frames: add(budget.used.frames, spend.frames, budget.limits.frames)?,
        audio_ms: add(budget.used.audio_ms, spend.audio_ms, budget.limits.audio_ms)?,
    };
    let operations = add(budget.operations, 1, budget.limits.operations)?;
    budget.used = next;
    budget.operations = operations;
    Ok(())
}

pub fn tools(_: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(Perception)]
}
struct Perception;
impl Tool for Perception {
    fn name(&self) -> &str {
        "perception"
    }
    fn schema(&self) -> Value {
        crate::tools::tool_schema(
            "perception",
            "Budgeted active perception: open a human-confirmed workspace-wide LIVE budget, step through visual/audio/video/document inspections, status, or close with confirmation. Decide the next discriminating selection from actual prior evidence; stop when resolved or budget exhausted. Direct supported producer calls share the same counters. Failed attempts consume units. Limits are inspection units, NOT a token/dollar cap; provider prices unknown. Budgets live only in this process and are shared across sessions using this workspace; restart requires explicitly opening a fresh budget. Not a security boundary against shell/browser/other tools and never grants execution or source access. Video remains opt-in. Whole-document PDF extraction is refused during a budget: use selected visual PDF pages. No hidden model calls.",
            json!({
                "action":{"type":"string","enum":["open","status","step","close"]},
                "question":{"type":"string","maxLength":4096},
                "limits":{"type":"object","properties":{"images":{"type":"integer","minimum":0,"maximum":128},"pages":{"type":"integer","minimum":0,"maximum":64},"frames":{"type":"integer","minimum":0,"maximum":64},"audio_ms":{"type":"integer","minimum":0,"maximum":600000},"operations":{"type":"integer","minimum":1,"maximum":128}},"required":["images","pages","frames","audio_ms","operations"],"additionalProperties":false},
                "tool":{"type":"string","enum":["visual","audio","video","document"]},"request":{"type":"object"},
                "reason":{"type":"string","maxLength":4096}
            }),
            &["action"],
        )
    }
    fn needs_approval(&self, args: &Value) -> bool {
        args["action"] != "status"
    }
    fn may_modify_workspace(&self, _: &Value) -> bool {
        false
    }
    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        self.call_streaming(ctx, args, &|_| {}, &AtomicBool::new(false))
    }
    fn call_streaming(
        &self,
        ctx: &ToolContext,
        args: &Value,
        _: &(dyn Fn(crate::tools::integrations::ext::ToolEvent) + Sync),
        cancel: &AtomicBool,
    ) -> ToolOutput {
        run(ctx, args, cancel).unwrap_or_else(|e| ToolOutput::err(format!("{e:#}")))
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    action: String,
    question: Option<String>,
    limits: Option<Limits>,
    tool: Option<String>,
    request: Option<Value>,
    reason: Option<String>,
}
fn nonempty(value: Option<&str>, name: &str) -> Result<String> {
    let value = value.with_context(|| format!("{name} is required"))?;
    ensure!(
        !value.trim().is_empty() && value.len() <= 4096,
        "{name} must contain 1..=4096 bytes"
    );
    Ok(value.into())
}
fn confirmed(ctx: &ToolContext, message: &str) -> Result<()> {
    ensure!(
        ctx.confirm.is_some_and(|confirm| confirm(message)),
        "inspection budget changes require actual user confirmation"
    );
    Ok(())
}
fn status(ws: &Workspace) -> Value {
    let all = budgets().lock().expect("perception budgets lock poisoned");
    json!({"budget":all.get(ws.root()),"scope":"live process, shared workspace; supported producers only; not token or money limits","cost":{"tokens":null,"money":null,"reason":"provider-dependent; inspection counts are not a price estimate"}})
}
fn run(ctx: &ToolContext, value: &Value, cancel: &AtomicBool) -> Result<ToolOutput> {
    let args: Args = serde_json::from_value(value.clone())?;
    ensure!(!cancel.load(Ordering::Relaxed), "perception cancelled");
    match args.action.as_str() {
        "open" => {
            let question = nonempty(args.question.as_deref(), "question")?;
            let limits = args.limits.context("explicit limits are required")?;
            ensure!(
                limits.images <= 128
                    && limits.pages <= 64
                    && limits.frames <= 64
                    && limits.audio_ms <= 600_000
                    && (1..=128).contains(&limits.operations),
                "inspection limits exceed supported bounds"
            );
            confirmed(
                ctx,
                &format!(
                    "Open a shared LIVE workspace inspection budget for {question:?}: {}? Units are not tokens/dollars. Existing execution approvals still apply; failed attempts consume units.",
                    serde_json::to_string(&limits)?
                ),
            )?;
            let mut all = budgets().lock().expect("perception budgets lock poisoned");
            ensure!(
                !all.contains_key(ctx.workspace.root()),
                "an active budget already exists; close it with human confirmation before replacing it"
            );
            ensure!(
                all.len() < 256,
                "too many live workspace budgets; close unused budgets"
            );
            all.insert(
                ctx.workspace.root().into(),
                Budget {
                    limits,
                    used: Spend::default(),
                    operations: 0,
                    question,
                },
            );
        }
        "status" => {}
        "close" => {
            let reason = nonempty(args.reason.as_deref(), "reason")?;
            confirmed(
                ctx,
                &format!(
                    "Close the workspace inspection budget ({reason})? Counters: {}. Subsequent direct inspections will no longer have this unit cap.",
                    status(ctx.workspace)
                ),
            )?;
            budgets()
                .lock()
                .expect("perception budgets lock poisoned")
                .remove(ctx.workspace.root());
        }
        "step" => {
            nonempty(args.reason.as_deref(), "reason")?;
            ensure!(
                budgets()
                    .lock()
                    .expect("perception budgets lock poisoned")
                    .contains_key(ctx.workspace.root()),
                "open a confirmed live budget before stepping"
            );
            let request = args.request.context("request is required")?;
            let tool = args.tool.context("tool is required")?;
            let mut output = match tool.as_str() {
                "visual" => crate::tools::fs::visual::execute(ctx, &request, cancel),
                "audio" => crate::tools::fs::audio::execute(ctx, &request, cancel),
                "video" => {
                    let tools = crate::tools::fs::video::tools(ctx.config);
                    tools
                        .first()
                        .context(
                            "video is disabled; enable experimental.video_analysis explicitly",
                        )?
                        .call_streaming(ctx, &request, &|_| {}, cancel)
                }
                "document" => crate::tools::fs::documents::analysis_tools(ctx.config)[0]
                    .call_streaming(ctx, &request, &|_| {}, cancel),
                _ => bail!("unsupported perception tool"),
            };
            output
                .text
                .push_str(&format!("\nInspection budget: {}", status(ctx.workspace)));
            return Ok(output);
        }
        _ => bail!("unknown perception action"),
    }
    Ok(ToolOutput::structured(status(ctx.workspace)))
}
/// Whole-document PDF parsing cannot honor a selected-page budget.
pub(crate) fn require_selected_pdf(ws: &Workspace) -> Result<()> {
    ensure!(
        !budgets()
            .lock()
            .expect("perception budgets lock poisoned")
            .contains_key(ws.root()),
        "active perception budget requires selected PDF pages through visual; whole-document extraction refused"
    );
    Ok(())
}
#[cfg(test)]
mod tests;
