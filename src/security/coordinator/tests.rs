use super::*;
use std::collections::VecDeque;
use std::fs;

use crate::security::contracts::{AccountRef, ModelRef, ValidationStatus};
use crate::security::preflight::{self, PlanRequest, TargetRequest};

fn paths(count: usize) -> Vec<String> {
    (0..count)
        .map(|index| format!("src/file{index:03}.rs"))
        .collect()
}

fn assignment(batches: &[Vec<String>]) -> String {
    let batches: Vec<Value> = batches
        .iter()
        .map(|files| json!({"files": files}))
        .collect();
    json!({"batches": batches, "coverage_notes": "ok"}).to_string()
}

#[test]
fn coordinator_prompt_is_the_native_body() {
    assert!(
        COORDINATOR_PROMPT
            .starts_with("Coordinate a software-security scan with this session's native tools.")
    );
    assert!(
        COORDINATOR_PROMPT
            .ends_with("NEVER return final success before it accepts canonical result.")
    );
}

#[test]
fn assign_chunks_batches_to_batch_size_and_keeps_order() {
    let inventory = paths(45);
    let (batches, unassigned) =
        assign(&inventory, &assignment(std::slice::from_ref(&inventory))).unwrap();
    let sizes: Vec<usize> = batches.iter().map(Vec::len).collect();
    assert_eq!(sizes, vec![20, 20, 5]);
    assert_eq!(batches.concat(), inventory);
    assert!(unassigned.is_empty());
}

#[test]
fn assign_drops_unknown_and_duplicate_paths_and_defers_the_rest() {
    let inventory = paths(3);
    let text = assignment(&[
        vec![
            inventory[0].clone(),
            "../escape.rs".into(),
            "missing.rs".into(),
        ],
        vec![inventory[0].clone(), inventory[1].clone()],
    ]);
    let (batches, unassigned) = assign(&inventory, &text).unwrap();
    assert_eq!(
        batches,
        vec![vec![inventory[0].clone()], vec![inventory[1].clone()]]
    );
    assert_eq!(unassigned, vec![inventory[2].clone()]);
}

#[test]
fn assign_rejects_non_json_coordinator_output() {
    let error = assign(&paths(1), "not json").unwrap_err();
    assert!(format!("{error:#}").contains("Security coordinator returned invalid output"));
}

type Reply = std::result::Result<String, String>;

fn ok(text: String) -> Reply {
    Ok(text)
}

fn report(paths: &[String]) -> String {
    json!({"coverage_summary": "reviewed", "reviewed_paths": paths, "findings": []}).to_string()
}

fn report_with_finding(path: &str) -> String {
    json!({
        "coverage_summary": "reviewed",
        "reviewed_paths": [path],
        "findings": [{
            "rule_id": "sql-injection", "title": "SQLi", "summary": "concat",
            "severity": "high", "confidence": "medium", "category": "injection",
            "locations": [{"path": path, "start_line": 1}], "cwe": ["CWE-89"], "evidence": []
        }]
    })
    .to_string()
}

/// Scripted reviewer. Records each prompt and the phase of the running scan. `cancel_after`
/// requests cancellation through the scan registry once that many prompts have run.
/// Scripted reviewer. Records each prompt and the phase of the running scan. `cancel_after`
/// requests cancellation through the scan registry once that many prompts have run.
/// `rewrite_after` overwrites a file once that many prompts have run, as if edited mid-scan.
struct Fake<'s> {
    store: &'s Store,
    replies: VecDeque<Reply>,
    prompts: Vec<String>,
    phases: Vec<Option<Phase>>,
    cancel_after: Option<usize>,
    rewrite_after: Option<(usize, std::path::PathBuf)>,
}

impl<'s> Fake<'s> {
    fn new(store: &'s Store, replies: Vec<Reply>) -> Self {
        Self {
            store,
            replies: replies.into(),
            prompts: Vec::new(),
            phases: Vec::new(),
            cancel_after: None,
            rewrite_after: None,
        }
    }
}

impl Reviewer for Fake<'_> {
    fn run_reviewer(&mut self, prompt: &str, _output_schema: &Value) -> Result<String> {
        self.prompts.push(prompt.to_owned());
        if let Some(scan) = self.store.scans()?.first() {
            self.phases.push(scan::phase(&scan.id));
            if self.cancel_after == Some(self.prompts.len()) {
                scan::request_cancel(&scan.id);
            }
        }
        if let Some((after, path)) = &self.rewrite_after
            && self.prompts.len() == *after
        {
            fs::write(path, "fn changed() {}\n").unwrap();
        }
        match self.replies.pop_front() {
            Some(Ok(text)) => Ok(text),
            Some(Err(message)) => bail!("{message}"),
            None => bail!("no scripted reply"),
        }
    }
}

fn git_in(repo: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

/// A temp Git repository tracking `files`, planned as a whole-repository scan.
fn fixture(files: &[String]) -> (tempfile::TempDir, Plan, Store) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    git_in(&repo, &["init", "-q"]);
    for file in files {
        let path = repo.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "fn main() {}\n").unwrap();
    }
    git_in(&repo, &["add", "-A"]);
    let output = dir.path().join("outputs");
    let request = PlanRequest {
        cwd: &repo,
        target: TargetRequest::Repository {
            include: vec![],
            exclude: vec![],
        },
        knowledge_bases: vec![],
        output_root: &output,
        archive_existing: false,
        model: ModelRef {
            provider: "openai".into(),
            model_id: "test-model".into(),
            thinking_level: None,
        },
        account: AccountRef {
            provider: "openai-codex".into(),
            credential_id: "test-account".into(),
        },
        config_material: json!({"model": "test-model"}),
        workflow_fingerprint: "test".into(),
    };
    let plan = preflight::create_plan(&request, "2024-01-01T00:00:00Z").unwrap();
    let store = Store::open(&dir.path().join("user"), &plan.repository_root).unwrap();
    (dir, plan, store)
}

fn chunks_of(inventory: &[String]) -> Vec<Vec<String>> {
    inventory
        .chunks(BATCH_SIZE)
        .map(<[String]>::to_vec)
        .collect()
}

#[test]
fn completes_after_each_batch_reports_its_files() {
    let inventory = paths(45);
    let (_dir, plan, store) = fixture(&inventory);
    let chunks = chunks_of(&inventory);
    let mut replies = vec![ok(assignment(&chunks))];
    replies.extend(chunks.iter().map(|chunk| ok(report(chunk))));
    let mut reviewer = Fake::new(&store, replies);
    let run = start(&store, &plan, &mut reviewer).unwrap();
    assert_eq!(run.scan.status, ScanStatus::Completed);
    assert!(run.deferred.is_empty());
    assert_eq!(reviewer.prompts.len(), 4);
    assert!(reviewer.prompts[1].contains(&inventory[0]));
    assert!(
        reviewer
            .phases
            .iter()
            .all(|phase| *phase == Some(Phase::Reviewing))
    );
    assert_eq!(
        store.load_scan(&run.scan.id).unwrap().status,
        ScanStatus::Completed
    );
    assert_eq!(scan::phase(&run.scan.id), None);
}

#[test]
fn partial_when_a_batch_review_fails() {
    let inventory = paths(25);
    let (_dir, plan, store) = fixture(&inventory);
    let chunks = chunks_of(&inventory);
    let replies = vec![
        ok(assignment(&chunks)),
        Err("reviewer offline".into()),
        ok(report(&chunks[1])),
    ];
    let mut reviewer = Fake::new(&store, replies);
    let run = start(&store, &plan, &mut reviewer).unwrap();
    assert_eq!(run.scan.status, ScanStatus::Partial);
    assert_eq!(run.deferred.len(), 20);
    assert!(run.deferred[0].contains("review failed: reviewer offline"));
    assert_eq!(
        store.load_scan(&run.scan.id).unwrap().status,
        ScanStatus::Partial
    );
}

#[test]
fn cancelled_when_cancel_is_requested_mid_run() {
    let inventory = paths(45);
    let (_dir, plan, store) = fixture(&inventory);
    let chunks = chunks_of(&inventory);
    let mut replies = vec![ok(assignment(&chunks))];
    replies.extend(chunks.iter().map(|chunk| ok(report(chunk))));
    let mut reviewer = Fake::new(&store, replies);
    reviewer.cancel_after = Some(2);
    let run = start(&store, &plan, &mut reviewer).unwrap();
    assert_eq!(run.scan.status, ScanStatus::Cancelled);
    assert_eq!(reviewer.prompts.len(), 2);
    assert_eq!(
        store.load_scan(&run.scan.id).unwrap().status,
        ScanStatus::Cancelled
    );
    assert!(!scan::is_active(&run.scan.id));
}

#[test]
fn failed_when_coordinator_output_is_invalid() {
    let inventory = paths(3);
    let (_dir, plan, store) = fixture(&inventory);
    let mut reviewer = Fake::new(&store, vec![ok("not json".into())]);
    let error = start(&store, &plan, &mut reviewer).err().unwrap();
    assert!(format!("{error:#}").contains("Security coordinator returned invalid output"));
    let scans = store.scans().unwrap();
    assert_eq!(scans.len(), 1);
    assert_eq!(scans[0].status, ScanStatus::Failed);
}

#[test]
fn validate_records_the_reviewer_verdict() {
    let inventory = paths(1);
    let (_dir, plan, store) = fixture(&inventory);
    let replies = vec![
        ok(assignment(std::slice::from_ref(&inventory))),
        ok(report_with_finding(&inventory[0])),
    ];
    let mut reviewer = Fake::new(&store, replies);
    let run = start(&store, &plan, &mut reviewer).unwrap();
    let finding_id = run.scan.findings[0].id.clone();

    let verdicts = [
        ("validated", ValidationStatus::Validated),
        ("rejected", ValidationStatus::Rejected),
    ];
    for (status, expected) in verdicts {
        let verdict = json!({"status": status, "summary": "checked", "evidence": []}).to_string();
        let mut verdict_reviewer = Fake::new(&store, vec![ok(verdict)]);
        let text =
            scan::validate(&store, &run.scan.id, &finding_id, &mut verdict_reviewer).unwrap();
        assert!(text.contains("checked"));
        let saved = store.load_scan(&run.scan.id).unwrap();
        assert_eq!(saved.findings[0].validation_status, Some(expected));
    }
}

#[test]
fn cancel_request_reaches_only_a_running_scan() {
    assert!(!scan::request_cancel("secscan_not_running"));
}

/// Commits everything staged in `repo` with a fixed identity.
fn commit_all(repo: &std::path::Path, message: &str) {
    git_in(
        repo,
        &[
            "-c",
            "user.name=Dial Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            message,
        ],
    );
}

/// A ref-diff fixture: `base` and `head` commits where `head` changes an in-scope file, an
/// excluded file, and deletes an in-scope file. The scope is `src`.
fn ref_fixture() -> (tempfile::TempDir, Plan, Store) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    for sub in ["src", "other", "docs"] {
        fs::create_dir_all(repo.join(sub)).unwrap();
    }
    git_in(&repo, &["init", "-q"]);
    fs::write(repo.join("src/keep.rs"), "fn keep() {}\n").unwrap();
    fs::write(repo.join("src/gone.rs"), "fn gone() {}\n").unwrap();
    fs::write(repo.join("other/secret.rs"), "fn secret() {}\n").unwrap();
    fs::write(repo.join("docs/readme.md"), "docs\n").unwrap();
    git_in(&repo, &["add", "-A"]);
    commit_all(&repo, "base");
    fs::write(repo.join("src/keep.rs"), "fn keep() { INCLUDED_CHANGE }\n").unwrap();
    fs::remove_file(repo.join("src/gone.rs")).unwrap();
    fs::write(
        repo.join("other/secret.rs"),
        "fn secret() { SECRET_EXCLUDED_MARKER }\n",
    )
    .unwrap();
    git_in(&repo, &["add", "-A"]);
    commit_all(&repo, "head");
    let output = dir.path().join("outputs");
    let request = PlanRequest {
        cwd: &repo,
        target: TargetRequest::RefDiff {
            base: "HEAD~1".into(),
            head: "HEAD".into(),
            include: vec!["src".into()],
            exclude: vec![],
        },
        knowledge_bases: vec![],
        output_root: &output,
        archive_existing: false,
        model: ModelRef {
            provider: "openai".into(),
            model_id: "test-model".into(),
            thinking_level: None,
        },
        account: AccountRef {
            provider: "openai-codex".into(),
            credential_id: "test-account".into(),
        },
        config_material: json!({"model": "test-model"}),
        workflow_fingerprint: "test".into(),
    };
    let plan = preflight::create_plan(&request, "2024-01-01T00:00:00Z").unwrap();
    let store = Store::open(&dir.path().join("user"), &plan.repository_root).unwrap();
    (dir, plan, store)
}

#[test]
fn scoped_ref_review_sends_only_in_scope_diff_material() {
    let (_dir, plan, store) = ref_fixture();
    let inventory = scope_files(&plan).unwrap().0;
    assert_eq!(inventory, vec!["src/keep.rs".to_owned()]);
    let replies = vec![
        ok(assignment(std::slice::from_ref(&inventory))),
        ok(report(&inventory)),
    ];
    let mut reviewer = Fake::new(&store, replies);
    let run = start(&store, &plan, &mut reviewer).unwrap();
    assert_eq!(run.scan.status, ScanStatus::Completed);
    assert_eq!(reviewer.prompts.len(), 2);
    for prompt in &reviewer.prompts {
        assert!(
            !prompt.contains("SECRET_EXCLUDED_MARKER"),
            "excluded bytes reached a prompt"
        );
        assert!(!prompt.contains("other/secret.rs"));
    }
    assert!(reviewer.prompts[1].contains("INCLUDED_CHANGE"));
}

#[test]
fn empty_ref_scope_has_no_inventory_or_summary() {
    let (_dir, mut plan, _store) = ref_fixture();
    plan.target.include_paths = vec!["docs".into()];
    let (files, summary) = scope_files(&plan).unwrap();
    assert!(files.is_empty());
    assert!(summary.is_none());
}

#[test]
fn source_changed_during_review_is_published_as_partial() {
    let inventory = paths(3);
    let (dir, plan, store) = fixture(&inventory);
    let chunks = chunks_of(&inventory);
    let replies = vec![ok(assignment(&chunks)), ok(report(&chunks[0]))];
    let mut reviewer = Fake::new(&store, replies);
    // Edit a reviewed file after the batch prompt has been handed to the reviewer.
    reviewer.rewrite_after = Some((2, dir.path().join("repo").join(&inventory[0])));
    let run = start(&store, &plan, &mut reviewer).unwrap();
    assert_eq!(run.scan.status, ScanStatus::Partial);
    assert!(
        run.deferred
            .iter()
            .any(|item| item.contains("source changed during review"))
    );
    assert_eq!(
        store.load_scan(&run.scan.id).unwrap().status,
        ScanStatus::Partial
    );
}
