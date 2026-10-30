//! Read-only review orchestration. Target bytes are captured before any model request.
use crate::agent::{Agent, Ui};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    path::Path,
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

fn capture(
    root: &Path,
    program: &str,
    args: &[&str],
    interrupt: &AtomicBool,
    deadline: Duration,
    max_bytes: u64,
) -> Result<String> {
    let mut command = Command::new(program);
    command.current_dir(root);
    if program == "git" {
        command.args([
            "--no-pager",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=NUL",
            "-c",
            "core.quotePath=false",
        ]);
    } else {
        command
            .env("GH_HOST", "github.com")
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_PAGER", "cat")
            .env("GH_DEBUG", "");
    }
    command.args(args).env("GIT_OPTIONAL_LOCKS", "0");
    let mut policy = crate::runtime::process::Policy::bounded(
        deadline,
        usize::try_from(max_bytes).context("review output bound is too large")?,
    );
    policy.fail_on_output_limit = true;
    policy.may_mutate = false;
    let output =
        crate::runtime::process::run_with_policy(command, None, policy, interrupt, &mut |_, _| {})
            .map_err(anyhow::Error::msg)?;
    // Credential helpers can echo secrets; review diagnostics never include raw stderr.
    anyhow::ensure!(
        output.success(),
        "{program} review capture failed (status {:?}, exit {:?}); check installation, authentication and target access",
        output.tool_status(),
        output.code()
    );
    String::from_utf8(output.stdout).context("Review requires UTF-8 target bytes")
}
fn git(root: &Path, args: &[&str], interrupt: &AtomicBool) -> Result<String> {
    capture(
        root,
        "git",
        args,
        interrupt,
        Duration::from_secs(30),
        64 * 1024 * 1024,
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PrRevision {
    url: String,
    base: String,
    head: String,
}

fn pr_selector(target: &str) -> Result<Option<(String, Option<String>)>> {
    let target = target.strip_prefix("pr ").unwrap_or(target);
    let parts: Vec<_> = if let Some(rest) = target.strip_prefix("pr://") {
        rest.split('/').collect()
    } else if let Some(rest) = target.strip_prefix("https://github.com/") {
        rest.split('/').collect()
    } else {
        return Ok(None);
    };
    let valid_number = |number: &str| {
        number.parse::<u64>().is_ok_and(|n| n > 0) && number.bytes().all(|b| b.is_ascii_digit())
    };
    let valid_name = |name: &str| {
        !name.is_empty()
            && name.len() <= 100
            && name != "."
            && name != ".."
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    let (number, repo) = match parts.as_slice() {
        [number] if target.starts_with("pr://") && valid_number(number) => {
            ((*number).to_owned(), None)
        }
        [owner, repo, number]
            if target.starts_with("pr://")
                && valid_name(owner)
                && valid_name(repo)
                && valid_number(number) =>
        {
            ((*number).to_owned(), Some(format!("{owner}/{repo}")))
        }
        [owner, repo, "pull", number]
            if target.starts_with("https://github.com/")
                && valid_name(owner)
                && valid_name(repo)
                && valid_number(number) =>
        {
            ((*number).to_owned(), Some(format!("{owner}/{repo}")))
        }
        _ => anyhow::bail!(
            "Invalid PR target: use pr://<number>, pr://<owner>/<repo>/<number>, or https://github.com/<owner>/<repo>/pull/<number>"
        ),
    };
    Ok(Some((number, repo)))
}
fn pr_revision(
    root: &Path,
    number: &str,
    repo: Option<&str>,
    interrupt: &AtomicBool,
) -> Result<PrRevision> {
    let mut args = vec!["pr", "view", number, "--json", "url,baseRefOid,headRefOid"];
    if let Some(repo) = repo {
        args.extend(["--repo", repo]);
    }
    let text = capture(
        root,
        "gh",
        &args,
        interrupt,
        Duration::from_secs(120),
        64 * 1024,
    )?;
    let value: Value =
        serde_json::from_str(&text).context("GitHub PR metadata is not valid JSON")?;
    let field = |key: &str| {
        value[key]
            .as_str()
            .map(str::to_owned)
            .context("GitHub PR metadata lacks required identity fields")
    };
    let revision = PrRevision {
        url: field("url")?,
        base: field("baseRefOid")?,
        head: field("headRefOid")?,
    };
    for sha in [&revision.base, &revision.head] {
        anyhow::ensure!(
            sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()),
            "GitHub returned an invalid commit identity"
        );
    }
    anyhow::ensure!(
        revision.url.starts_with("https://github.com/"),
        "GitHub returned an invalid PR URL"
    );
    let (returned_number, returned_repo) =
        pr_selector(&revision.url)?.context("GitHub returned an invalid PR URL")?;
    anyhow::ensure!(
        returned_number.parse::<u64>()? == number.parse::<u64>()?
            && repo.is_none_or(|repo| returned_repo
                .as_deref()
                .is_some_and(|actual| actual.eq_ignore_ascii_case(repo))),
        "GitHub returned metadata for a different PR target"
    );
    Ok(revision)
}
fn pr_snapshot(
    root: &Path,
    number: &str,
    repo: Option<&str>,
    interrupt: &AtomicBool,
) -> Result<(String, PrRevision)> {
    let before = pr_revision(root, number, repo, interrupt)?;
    let mut args = vec!["pr", "diff", number, "--color=never"];
    if let Some(repo) = repo {
        args.extend(["--repo", repo]);
    }
    let diff = capture(
        root,
        "gh",
        &args,
        interrupt,
        Duration::from_secs(120),
        64 * 1024 * 1024,
    )?;
    let after = pr_revision(root, number, repo, interrupt)?;
    anyhow::ensure!(
        before == after,
        "PR changed during capture; no review was performed. Request a new snapshot."
    );
    Ok((diff, before))
}
fn fingerprint(text: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(text.as_bytes()))
}
fn snapshot(root: &Path, target: &str, interrupt: &AtomicBool) -> Result<String> {
    let git = |root: &Path, args: &[&str]| git(root, args, interrupt);
    let words: Vec<_> = target.split_whitespace().collect();
    let resolve = |reference: &str| {
        git(
            root,
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{reference}^{{commit}}"),
            ],
        )
        .map(|s| s.trim().to_owned())
    };
    let mut diff = match words.as_slice() {
        [] | ["uncommitted"] => {
            let staged = git(
                root,
                &[
                    "diff",
                    "--cached",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--find-renames",
                    "--",
                ],
            )?;
            let unstaged = git(
                root,
                &[
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--find-renames",
                    "--",
                ],
            )?;
            if staged.is_empty() && unstaged.is_empty() {
                String::new()
            } else {
                format!("STAGED SNAPSHOT:\n{staged}\nUNSTAGED SNAPSHOT:\n{unstaged}")
            }
        }
        ["base", reference] => {
            let base = resolve(reference)?;
            let merge = git(root, &["merge-base", &base, "HEAD"])?;
            git(
                root,
                &[
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--find-renames",
                    merge.trim(),
                    "HEAD",
                    "--",
                ],
            )?
        }
        ["ref", reference] => {
            let reference = resolve(reference)?;
            git(
                root,
                &[
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--find-renames",
                    &reference,
                    "HEAD",
                    "--",
                ],
            )?
        }
        ["commit", reference] => {
            let reference = resolve(reference)?;
            git(
                root,
                &[
                    "show",
                    "--format=",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--find-renames",
                    &reference,
                    "--",
                ],
            )?
        }
        _ => anyhow::bail!(
            "usage: /review [uncommitted | base <ref> | ref <ref> | commit <ref> | pr://<number> | pr://<owner>/<repo>/<number> | https://github.com/<owner>/<repo>/pull/<number>]"
        ),
    };
    if words.is_empty() || words == ["uncommitted"] {
        let top = Path::new(git(root, &["rev-parse", "--show-toplevel"])?.trim()).to_path_buf();
        for path in git(&top, &["ls-files", "--others", "--exclude-standard", "-z"])?
            .split('\0')
            .filter(|p| !p.is_empty())
        {
            anyhow::ensure!(!interrupt.load(Ordering::SeqCst), "Review cancelled");
            let full = top.join(path);
            let meta = std::fs::symlink_metadata(&full)?;
            if meta.file_type().is_symlink() {
                diff.push_str(&format!(
                    "\ndiff --git a/{path} b/{path}\nnew symlink target {:?}\n",
                    std::fs::read_link(&full)?
                ));
                continue;
            }
            anyhow::ensure!(
                meta.file_type().is_file(),
                "Untracked target {path:?} is not a regular file"
            );
            anyhow::ensure!(
                meta.len() <= 64 * 1024 * 1024 && diff.len() as u64 <= 64 * 1024 * 1024,
                "Untracked target {path:?} exceeds the 64 MiB review diff limit"
            );
            let bytes = std::fs::read(full)?;
            let text = match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(error) => {
                    use sha2::{Digest, Sha256};
                    diff.push_str(&format!("\ndiff --git a/{path} b/{path}\nUntracked binary file; no textual review available. SHA256:{:x}\n", Sha256::digest(error.as_bytes())));
                    continue;
                }
            };
            diff.push_str(&format!("\ndiff --git a/{path} b/{path}\nnew file\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{} @@\n", text.lines().count()));
            for line in text.lines() {
                diff.push('+');
                diff.push_str(line);
                diff.push('\n');
            }
        }
    }
    Ok(diff)
}
fn schema() -> Value {
    json!({"type":"object","properties":{
        "verdict":{"type":"string","enum":["findings","no_findings","inconclusive"]},
        "summary":{"type":"string"},
        "findings":{"type":"array","items":{"type":"object","properties":{
            "priority":{"type":"integer","minimum":0,"maximum":3},"confidence":{"type":"number","minimum":0,"maximum":1},
            "path":{"type":"string","minLength":1},"start_line":{"type":"integer","minimum":1},"end_line":{"type":"integer","minimum":1},
            "title":{"type":"string"},"body":{"type":"string"}},"required":["priority","confidence","path","start_line","end_line","title","body"],"additionalProperties":false}}
    },"required":["verdict","summary","findings"],"additionalProperties":false})
}
impl Agent<'_> {
    pub fn review_target(
        &mut self,
        target: &str,
        instructions: &str,
        ui: &mut dyn Ui,
    ) -> Result<String> {
        let root = self.workspace.root().to_path_buf();
        let interrupt = self.interrupt.clone();
        let remote = pr_selector(target)?;
        let (captured, remote_revision, workspace_before, head) = if let Some((number, repo)) =
            &remote
        {
            anyhow::ensure!(
                !self.hermetic
                    && self.config.sandbox.mode != crate::config::types::SandboxMode::Strict
                    && (self.config.sandbox.mode == crate::config::types::SandboxMode::Off
                        || self.config.sandbox.allow_network),
                "PR review requires network access; the active sandbox policy forbids it"
            );
            // The host makes this call itself, so the git integration must be enabled and allowed
            // under the current mode; its schema need not be in the model's offer.
            anyhow::ensure!(
                self.catalog().iter().any(|tool| tool == "git") && self.tool_eligible("git"),
                "PR review requires the enabled git tool network policy"
            );
            let call = crate::llm::ToolCall {
                id: "review-pr-snapshot".into(),
                name: "git".into(),
                arguments: json!({"action":"review_pr_snapshot","number":number,"repo":repo,"target":target}),
                malformed_arguments: false,
            };
            self.authorize_call(&call, ui)
                .map_err(|denial| anyhow::anyhow!(denial.message()))?;
            let (diff, revision) = crate::agent::safety::hooks::with_heartbeat(ui, || {
                pr_snapshot(&root, number, repo.as_deref(), &interrupt)
            })?;
            (diff, Some(revision), String::new(), String::new())
        } else {
            let diff = crate::agent::safety::hooks::with_heartbeat(ui, || {
                snapshot(&root, target, &interrupt)
            })?;
            let workspace = crate::agent::safety::hooks::with_heartbeat(ui, || {
                snapshot(&root, "uncommitted", &interrupt)
            })?;
            let head = crate::agent::safety::hooks::with_heartbeat(ui, || {
                git(&root, &["rev-parse", "--verify", "HEAD"], &interrupt)
            })
            .unwrap_or_else(|_| "unborn HEAD".to_owned());
            (diff, None, workspace, head)
        };
        let identity = match &remote_revision {
            Some(revision) => format!(
                "{}:base={}:head={}:diff={}",
                revision.url,
                revision.base,
                revision.head,
                fingerprint(&captured)
            ),
            None => format!(
                "{}:{}:{}",
                if target.is_empty() {
                    "uncommitted"
                } else {
                    target
                },
                head.trim(),
                fingerprint(&captured)
            ),
        };
        if captured.is_empty() {
            return Ok(format!(
                "Target {identity}\nVerdict: no changes to review (no model verification performed)."
            ));
        }
        let mut backend = self.backend.take().context("backend is already in use")?;
        let result = (|| -> Result<Vec<report::ChunkReview>> {
            // File-boundary chunks keep every task inside the runner's prompt bound without
            // dropping target bytes; the task batch runs them concurrently up to
            // `task.max_concurrency`, and the interrupt flag cancels every running chunk.
            let ranges = report::chunk_diff(&captured, report::CHUNK_BYTES);
            let count = ranges.len();
            let mut reviews = Vec::with_capacity(count);
            for group in ranges.chunks(crate::agent::workers::subagents::MAX_BATCH) {
                if self.interrupt.load(Ordering::SeqCst) {
                    anyhow::bail!("Review cancelled");
                }
                let tasks: Vec<Value> = group
                    .iter()
                    .enumerate()
                    .map(|(position, range)| {
                        let number = reviews.len() + position + 1;
                        let prompt = format!(
                            "Review immutable target {identity}, chunk {number} of {count}, diff bytes {}..{} of {}. Diff fragments can split hunks; never infer an issue from missing fragment context. Read workspace files only as supplementary context, not as replacement for these captured target bytes. No commands, checks, changes, commits or pushes. Return structured findings only for introduced concrete defects. P0=critical, P1=high, P2=normal, P3=low; confidence is a model judgment, not proof. Paths must be target-relative; ranges are inclusive and end >= start. Explicit verdict required. Do not claim tests ran. Additional review guidance: {}\nIMMUTABLE DIFF:\n{}",
                            range.start,
                            range.end,
                            captured.len(),
                            crate::agent::workers::subagents::clip(instructions, 4000),
                            &captured[range.clone()]
                        );
                        let prompt = if remote_revision.is_some() {
                            format!(
                                "This is a frozen remote GitHub PR diff, NOT the local checkout. No local file reads are available. Review only supplied exact diff bytes; report inconclusive when missing full-file context prevents a finding. {prompt}"
                            )
                        } else {
                            prompt
                        };
                        json!({"agent":"reviewer","name":format!("review {number}/{count}"),"task":prompt,"solution_space":"review of a captured change: the diff is given, findings are open","output_schema":schema()})
                    })
                    .collect();
                let args = json!({ "tasks": tasks });
                let output = if remote_revision.is_some() {
                    let mut harness = self.version.harness.clone();
                    harness.tools.clear();
                    let env = crate::agent::workers::subagents::Env {
                        config: &self.auth_config,
                        workspace: &self.workspace,
                        ext: &self.ext,
                        layout: self.layout.as_ref(),
                        harness: &harness,
                        interrupt: &self.interrupt,
                        usage: Some(&self.usage_scope),
                        context_cap: Some(self.context_window()),
                        memory: None,
                        plan_mode: self.plan_mode,
                        agents_messages: &self.agents_messages,
                    };
                    let parent = crate::agent::workers::subagents::Parent {
                        env,
                        approval: self.approval,
                        always: &mut self.always,
                        main: &mut backend,
                        side: &mut self.subagent_backend,
                        cost: &mut self.session_cost,
                        adapter: self.version.adapter.as_deref(),
                        handoff: Default::default(),
                    };
                    crate::agent::workers::subagents::run(
                        parent,
                        Some(self.session.id()),
                        &args,
                        ui,
                    )
                } else {
                    self.run_task(&mut backend, &args, ui)
                };
                let jobs = output
                    .data
                    .as_ref()
                    .and_then(|data| data["jobs"].as_array())
                    .with_context(|| output.text.clone())?;
                for (position, range) in group.iter().enumerate() {
                    reviews.push(report::from_job(
                        range.clone(),
                        jobs.get(position).unwrap_or(&Value::Null),
                        &captured,
                        &identity,
                    ));
                }
                if self.interrupt.load(Ordering::SeqCst) {
                    anyhow::bail!("Review cancelled");
                }
            }
            Ok(reviews)
        })();
        self.backend = Some(backend);
        let reviews = result?;
        let stale = if let (Some((number, repo)), Some(revision)) = (&remote, &remote_revision) {
            crate::agent::safety::hooks::with_heartbeat(ui, || {
                pr_revision(&root, number, repo.as_deref(), &interrupt)
            })
            .map_or(true, |now| now != *revision)
        } else {
            crate::agent::safety::hooks::with_heartbeat(ui, || snapshot(&root, target, &interrupt))
                .map_or(true, |now| now != captured)
                || crate::agent::safety::hooks::with_heartbeat(ui, || {
                    snapshot(&root, "uncommitted", &interrupt)
                })
                .map_or(true, |now| now != workspace_before)
                || crate::agent::safety::hooks::with_heartbeat(ui, || {
                    git(&root, &["rev-parse", "--verify", "HEAD"], &interrupt)
                })
                .unwrap_or_else(|_| "unborn HEAD".to_owned())
                    != head
        };
        let merged = report::Merged::new(&reviews, stale);
        Ok(format!(
            "{}\nMachine-readable report:\n{}",
            report::render(&identity, stale, &reviews, &merged),
            serde_json::to_string_pretty(&report::to_json(&identity, stale, &reviews, &merged))?
        ))
    }
}

mod report;
