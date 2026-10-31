use super::*;
impl Terminal for () {
    fn prevent_completion(&mut self, _: &str) {}
}
impl Terminal for u32 {
    fn prevent_completion(&mut self, _: &str) {}
}

fn failing(name: &str) -> ToolCall {
    ToolCall {
        id: "first".into(),
        name: name.into(),
        arguments: serde_json::json!({}),
        malformed_arguments: false,
    }
}

#[test]
fn first_failure_preserves_the_diagnostic_and_adds_compact_recovery_for_any_tool() {
    use crate::agent::run::failure_guidance::{DIAGNOSE_FIRST, FIRST_FAILURE};
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    for (name, output) in [
        ("read", ToolOutput::err("missing file: a.rs")),
        (
            "custom-mcp-tool",
            ToolOutput::err("remote service unavailable"),
        ),
        (
            "write",
            ToolOutput::err("partial commit").with_effects(Effects::Unknown),
        ),
    ] {
        let call = failing(name);
        let mut engine = Engine::default();
        let diagnostic = output.text.clone();
        let mut output = output;
        let (_, _, stop) = engine.settle(&workspace, &call, &mut output, false, false, |_| false);
        assert!(output.text.starts_with(&diagnostic));
        assert!(output.text.contains("[Dial recovery:"));
        let uncertain = output.execution.effects == Effects::Unknown;
        // Uncertain effects always carry the full contract; other first failures stay short.
        assert_eq!(output.text.contains(DIAGNOSE_FIRST), uncertain, "{name}");
        assert_eq!(output.text.contains(FIRST_FAILURE), !uncertain, "{name}");
        assert_eq!(stop.is_some(), uncertain);
    }
}

#[test]
fn repeated_failure_carries_the_full_contract_and_success_resets_it() {
    use crate::agent::run::failure_guidance::{DIAGNOSE_FIRST, FIRST_FAILURE};
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let mut engine = Engine::default();
    let mut settle = |name: &str, output: ToolOutput| {
        let mut output = output;
        engine.settle(
            &workspace,
            &failing(name),
            &mut output,
            false,
            false,
            |_| false,
        );
        output.text
    };
    let first = settle("read", ToolOutput::err("missing file: a.rs"));
    assert!(first.contains("[Dial recovery: tool_failure] ") && first.contains(FIRST_FAILURE));
    assert!(!first.contains(DIAGNOSE_FIRST));
    // Another tool's failure is its own first failure.
    assert!(settle("grep", ToolOutput::err("bad regex")).contains(FIRST_FAILURE));
    let second = settle("read", ToolOutput::err("missing file: b.rs"));
    assert!(
        second.contains("[Dial recovery: tool_failure] "),
        "{second}"
    );
    assert!(second.contains(DIAGNOSE_FIRST) && !second.contains(FIRST_FAILURE));
    assert!(second.contains("2 times in a row"), "{second}");
    assert!(settle("read", ToolOutput::err("missing")).contains("3 times in a row"));
    // A success of the tool ends its streak.
    settle("read", ToolOutput::ok("1:fn main() {}".into()));
    let again = settle("read", ToolOutput::err("missing file: c.rs"));
    assert!(again.contains(FIRST_FAILURE) && !again.contains(DIAGNOSE_FIRST));
}

#[test]
fn parent_and_worker_settlement_produce_identical_feedback_bytes() {
    // Both roles settle through `Engine`; the same history yields byte-identical feedback, so
    // stored results stay byte-stable across replays and roles.
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let run = || {
        let mut engine = Engine::default();
        ["edit", "edit", "bash", "edit"]
            .into_iter()
            .map(|name| {
                let mut output = ToolOutput::err("stale anchors");
                engine.settle(
                    &workspace,
                    &failing(name),
                    &mut output,
                    false,
                    false,
                    |_| false,
                );
                output.text
            })
            .collect::<Vec<_>>()
    };
    let (parent, worker) = (run(), run());
    assert_eq!(parent, worker);
    assert!(parent[0].contains("[Dial recovery: source_or_call_contract] "));
    assert!(parent[3].contains("3 times in a row"));
}
#[test]
fn shared_driver_cannot_publish_a_completed_report_after_uncertain_effects() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let call = ToolCall {
        id: "effect".into(),
        name: "write".into(),
        arguments: serde_json::json!({}),
        malformed_arguments: false,
    };
    let mut engine = Engine::default();
    let mut output = ToolOutput::err("execution disappeared").with_effects(Effects::Unknown);
    engine.settle(&workspace, &call, &mut output, false, false, |_| false);
    let report = engine
        .drive(|_| {
            Ok(Tick::Finished(crate::agent::TurnReport {
                text: "candidate report".into(),
                outcome: crate::agent::TurnOutcome::Completed,
                steps: 1,
                interrupted: false,
                trajectory: None,
            }))
        })
        .unwrap()
        .unwrap();
    assert_eq!(report.outcome, crate::agent::TurnOutcome::Incomplete);
    assert!(report.text.contains("uncertain effects"));
}
#[test]
fn shared_driver_refuses_automatic_retry_after_an_uncertain_effect() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let call = ToolCall {
        id: "effect".into(),
        name: "write".into(),
        arguments: serde_json::json!({}),
        malformed_arguments: false,
    };
    let mut engine = Engine::default();
    let mut cycles = 0;
    let result = engine.drive::<()>(|engine| {
        cycles += 1;
        let mut output = ToolOutput::err("process vanished").with_effects(Effects::Unknown);
        engine.settle(&workspace, &call, &mut output, false, false, |_| false);
        Ok(Tick::Again)
    });
    assert!(result.is_err());
    assert_eq!(cycles, 1);
}
#[test]
fn productive_driver_has_no_hidden_global_step_limit() {
    let mut engine = Engine::default();
    let mut cycles = 0;
    let result = engine
        .drive(|_| {
            cycles += 1;
            Ok(if cycles == 128 {
                Tick::Finished(cycles)
            } else {
                Tick::Again
            })
        })
        .unwrap();
    assert_eq!(result, Some(128));
}

const LOOP_NOTE: &str = "[Dial: edit loop]";

/// Commits real file contents and settles tool results carrying the controller-owned
/// `changed_paths`, as the transaction owner reports them.
struct Edits {
    dir: tempfile::TempDir,
    workspace: Workspace,
    engine: Engine,
    calls: u32,
}
impl Edits {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        Self {
            dir,
            workspace,
            engine: Engine::default(),
            calls: 0,
        }
    }
    fn path(&self, file: &str) -> std::path::PathBuf {
        self.workspace.root().join(file)
    }
    fn call(&mut self, name: &str, arguments: serde_json::Value) -> ToolCall {
        self.calls += 1;
        ToolCall {
            id: format!("call{}", self.calls),
            name: name.into(),
            arguments,
            malformed_arguments: false,
        }
    }
    /// The call that writes `content` to `file`; equal edits share an identity.
    fn edit_call(&mut self, tool: &str, file: &str, content: &str) -> ToolCall {
        self.call(tool, serde_json::json!({"path": file, "content": content}))
    }
    /// Settled model-facing text and any stop message.
    fn settle(
        &mut self,
        call: &ToolCall,
        changed: Vec<std::path::PathBuf>,
        mut output: ToolOutput,
    ) -> (String, Option<String>) {
        output.execution.changed_paths = changed;
        let stop = self
            .engine
            .settle(&self.workspace, call, &mut output, false, false, |_| false)
            .2;
        (output.text, stop)
    }
    fn write(&self, file: &str, content: &str) {
        std::fs::write(self.path(file), content).unwrap();
    }
    fn commit_as(&mut self, tool: &str, file: &str, content: &str) -> String {
        let call = self.edit_call(tool, file, content);
        assert!(
            self.engine.rejection_at(&self.workspace, &call).is_none(),
            "{tool} {file} {content} was refused"
        );
        self.write(file, content);
        let changed = vec![self.path(file)];
        let (text, stop) =
            self.settle(&call, changed, ToolOutput::ok(format!("applied {content}")));
        assert_eq!(stop, None, "an edit loop never ends the turn");
        text
    }
    fn commit(&mut self, file: &str, content: &str) -> String {
        self.commit_as("edit", file, content)
    }
    /// Index of the commit reported as an edit loop, if any, for one character per version.
    fn sequence(&mut self, file: &str, versions: &str) -> Option<usize> {
        let notes: Vec<bool> = versions
            .chars()
            .map(|version| self.commit(file, &version.to_string()).contains(LOOP_NOTE))
            .collect();
        notes.iter().position(|note| *note)
    }
    fn refused(&mut self, tool: &str, file: &str, content: &str) -> Option<String> {
        let call = self.edit_call(tool, file, content);
        self.engine.rejection_at(&self.workspace, &call)
    }
    fn neutral_progress(&mut self) {
        // A successful read and a resolved todo used to clear recovery state.
        let read = self.call("read", serde_json::json!({"path": "other.txt"}));
        let mut output = ToolOutput::ok("1:other".to_owned());
        self.engine
            .settle(&self.workspace, &read, &mut output, false, false, |_| false);
        let todo = self.call("todo", serde_json::json!({"op": "done"}));
        let mut output = ToolOutput::ok("done".to_owned());
        self.engine
            .settle(&self.workspace, &todo, &mut output, false, true, |_| false);
    }
}

fn completed_report() -> crate::agent::TurnReport {
    crate::agent::TurnReport {
        text: "candidate report".into(),
        outcome: crate::agent::TurnOutcome::Completed,
        steps: 1,
        interrupted: false,
        trajectory: None,
    }
}

#[test]
fn cycling_edits_are_reported_to_the_model_and_the_turn_keeps_going() {
    let mut edits = Edits::new();
    assert_eq!(edits.sequence("Customizer.vue", "ABCDABCDA"), Some(8));
    let mut edits = Edits::new();
    edits.sequence("Customizer.vue", "ABAB");
    let text = edits.commit("Customizer.vue", "A");
    assert!(text.starts_with("applied A"), "{text}");
    for expected in [
        LOOP_NOTE,
        "`Customizer.vue`",
        "undoing each other",
        "turn continues",
        "2 edit call(s)",
        "refused if repeated",
        "continue with the rest of the task",
    ] {
        assert!(text.contains(expected), "{expected}: {text}");
    }
    assert!(edits.engine.pending_refusal().is_none());
    // Automatic continuation and a completed report both remain possible.
    let mut rounds = 0;
    let report = edits
        .engine
        .drive(|_| {
            rounds += 1;
            Ok(if rounds < 3 {
                Tick::Again
            } else {
                Tick::Finished(completed_report())
            })
        })
        .unwrap()
        .unwrap();
    assert_eq!(report.outcome, crate::agent::TurnOutcome::Completed);
}

#[test]
fn loop_edits_are_refused_while_different_edits_files_and_setup_stay_available() {
    let mut edits = Edits::new();
    // `X` is reached once before the loop; only the alternating edits are refused.
    assert_eq!(edits.sequence("a.txt", "XABABA"), Some(5));
    for loop_edit in ["A", "B"] {
        let refusal = edits.refused("edit", "a.txt", loop_edit).unwrap();
        assert!(
            refusal.starts_with("This call was not executed"),
            "{refusal}"
        );
        assert!(refusal.contains("`a.txt`"), "{refusal}");
        assert!(
            refusal.contains("continue with the rest of the task"),
            "{refusal}"
        );
    }
    assert!(edits.refused("edit", "a.txt", "X").is_none());
    assert!(edits.refused("edit", "a.txt", "final").is_none());
    assert!(
        edits.refused("write", "a.txt", "A").is_none(),
        "another tool is another call"
    );
    assert!(edits.refused("edit", "b.txt", "A").is_none());
    let read = edits.call("read", serde_json::json!({"path": "a.txt"}));
    assert!(edits.engine.rejection_at(&edits.workspace, &read).is_none());
    // A different, complete edit lands and the model can carry on editing the file.
    assert!(!edits.commit("a.txt", "final").contains(LOOP_NOTE));
    assert!(!edits.commit("a.txt", "final, refined").contains(LOOP_NOTE));
    assert!(
        edits.refused("edit", "a.txt", "A").is_some(),
        "refusals last for the turn"
    );
}

#[test]
fn one_revert_and_unchanged_receipts_are_not_cycles() {
    let mut edits = Edits::new();
    assert_eq!(edits.sequence("a.txt", "ABA"), None);
    let mut edits = Edits::new();
    // Consecutive identical versions are one visit: A, B, A.
    assert_eq!(edits.sequence("a.txt", "AAABBBAAA"), None);
    assert_eq!(edits.sequence("a.txt", "B"), None);
    assert_eq!(edits.sequence("a.txt", "C"), None);
    assert_eq!(edits.sequence("a.txt", "A"), Some(0));
}

#[test]
fn reverting_to_earlier_content_is_not_progress_but_new_content_is() {
    let mut edits = Edits::new();
    let failing = edits.call("bash", serde_json::json!({"command": "cargo check"}));
    edits.commit("a.txt", "A");
    edits.commit("a.txt", "B");
    edits.engine.arm(&failing, "failed check");
    // Returning to A creates nothing new, so the failed check stays withheld.
    edits.commit("a.txt", "A");
    assert!(
        edits
            .engine
            .rejection_at(&edits.workspace, &failing)
            .is_some()
    );
    edits.commit("a.txt", "A");
    assert!(
        edits
            .engine
            .rejection_at(&edits.workspace, &failing)
            .is_some()
    );
    edits.commit("a.txt", "C");
    assert!(
        edits
            .engine
            .rejection_at(&edits.workspace, &failing)
            .is_none()
    );
    // Deleted or untracked targets still count as real changes.
    edits.engine.arm(&failing, "failed check");
    let delete = edits.call("bash", serde_json::json!({"command": "rm gone.txt"}));
    let missing = edits.path("gone.txt");
    edits.settle(&delete, vec![missing], ToolOutput::ok("removed".to_owned()));
    assert!(
        edits
            .engine
            .rejection_at(&edits.workspace, &failing)
            .is_none()
    );
}

#[test]
fn duplicate_and_aliased_paths_share_one_history() {
    let mut edits = Edits::new();
    let canonical = edits.path("a.txt");
    // On macOS the temp dir is reached through `/var` while the root is `/private/var`.
    let aliases = [
        std::path::PathBuf::from("a.txt"),
        edits.dir.path().join("a.txt"),
        canonical.clone(),
        edits.dir.path().join(".").join("a.txt"),
        std::path::PathBuf::from("./a.txt"),
    ];
    let mut notes = Vec::new();
    for (version, alias) in "ABABA".chars().zip(aliases) {
        let call = edits.edit_call("edit", "a.txt", &version.to_string());
        edits.write("a.txt", &version.to_string());
        let changed = vec![alias.clone(), alias, canonical.clone()];
        let (text, _) = edits.settle(&call, changed, ToolOutput::ok(format!("applied {version}")));
        notes.push(text.matches(LOOP_NOTE).count());
    }
    assert_eq!(notes, [0, 0, 0, 0, 1]);
}

#[test]
fn equal_contents_in_different_files_do_not_share_history() {
    let mut edits = Edits::new();
    for version in ["A", "B", "A"] {
        assert!(!edits.commit("a.txt", version).contains(LOOP_NOTE));
        assert!(!edits.commit("b.txt", version).contains(LOOP_NOTE));
    }
    // The same digest in another file is not a revisit of this one.
    assert!(!edits.commit("c.txt", "A").contains(LOOP_NOTE));
}

#[test]
fn unrelated_success_reads_and_todos_do_not_erase_cycle_history() {
    let mut edits = Edits::new();
    for (index, version) in "ABAB".chars().enumerate() {
        assert!(
            !edits
                .commit("a.txt", &version.to_string())
                .contains(LOOP_NOTE)
        );
        edits.neutral_progress();
        assert!(
            !edits
                .commit("b.txt", &format!("novel {index}"))
                .contains(LOOP_NOTE)
        );
    }
    assert!(edits.commit("a.txt", "A").contains(LOOP_NOTE));
}

#[test]
fn mutation_tool_names_and_post_commit_errors_cannot_bypass_the_cycle_guard() {
    let mut edits = Edits::new();
    for (tool, version) in [
        ("edit", "A"),
        ("write", "B"),
        ("ast_edit", "A"),
        ("lsp", "B"),
    ] {
        assert!(!edits.commit_as(tool, "a.txt", version).contains(LOOP_NOTE));
    }
    // The tool reports a failure after its commit; the committed content is still observed.
    let call = edits.edit_call("write", "a.txt", "A2");
    edits.write("a.txt", "A");
    let path = edits.path("a.txt");
    let (text, stop) = edits.settle(
        &call,
        vec![path],
        ToolOutput::err("formatter failed after the write committed"),
    );
    assert!(text.contains(LOOP_NOTE) && text.contains("a.txt"), "{text}");
    assert_eq!(stop, None);
    for (tool, version) in [
        ("edit", "A"),
        ("write", "B"),
        ("ast_edit", "A"),
        ("lsp", "B"),
    ] {
        assert!(edits.refused(tool, "a.txt", version).is_some(), "{tool}");
    }
}

#[test]
fn unknown_effects_keep_their_stop_over_edit_cycles() {
    let mut edits = Edits::new();
    edits.sequence("a.txt", "ABAB");
    let call = edits.edit_call("edit", "a.txt", "A");
    edits.write("a.txt", "A");
    let path = edits.path("a.txt");
    let (text, stop) = edits.settle(
        &call,
        vec![path],
        ToolOutput::err("execution disappeared").with_effects(Effects::Unknown),
    );
    assert!(stop.unwrap().contains("uncertain effects"));
    assert!(!text.contains(LOOP_NOTE), "{text}");
    assert!(
        edits
            .engine
            .pending_refusal()
            .unwrap()
            .contains("uncertain")
    );
    let report = edits
        .engine
        .drive(|_| Ok(Tick::Finished(completed_report())))
        .unwrap()
        .unwrap();
    assert_eq!(report.outcome, crate::agent::TurnOutcome::Incomplete);
}

#[test]
fn continuing_after_uncertain_effects_is_a_typed_refusal_adapters_can_stop_on() {
    let mut edits = Edits::new();
    let call = edits.edit_call("edit", "a.txt", "A");
    edits.settle(
        &call,
        vec![edits.path("a.txt")],
        ToolOutput::err("execution disappeared").with_effects(Effects::Unknown),
    );
    let error = edits
        .engine
        .drive(|_| Ok(Tick::<crate::agent::TurnReport>::Again))
        .expect_err("an automatic continuation after uncertain effects is refused");
    assert!(
        error.downcast_ref::<UncertainRefusal>().is_some(),
        "{error:#}"
    );
}

#[test]
fn reset_from_new_steering_clears_turn_local_cycle_state() {
    let mut edits = Edits::new();
    assert_eq!(edits.sequence("a.txt", "ABABA"), Some(4));
    edits.engine.transition(RecoveryTransition::Steering);
    assert!(edits.refused("edit", "a.txt", "B").is_none());
    assert_eq!(edits.sequence("a.txt", "BABA"), None);
}

#[test]
fn many_novel_versions_and_loops_stay_bounded() {
    let mut edits = Edits::new();
    for version in 0..300 {
        assert!(
            !edits
                .commit("a.txt", &format!("v{version}"))
                .contains(LOOP_NOTE)
        );
    }
    for file in 0..100 {
        assert!(
            !edits
                .commit(&format!("f{file}.txt"), "x")
                .contains(LOOP_NOTE)
        );
    }
    // Each two-state loop refuses two calls; the refused set stays bounded too.
    for file in 0..40 {
        assert_eq!(edits.sequence(&format!("loop{file}.txt"), "ABABA"), Some(4));
    }
    assert!(edits.engine.pending_refusal().is_none());
    let (files, versions, refused) = edits.engine.cycles.retained();
    assert!(
        files <= 64 && versions <= 16 && refused <= 64,
        "{files} files, {versions} versions, {refused} refused calls"
    );
    assert!(edits.refused("edit", "loop39.txt", "A").is_some());
}

#[test]
fn continuation_retains_cycle_restrictions_but_allows_a_different_approach() {
    let mut edits = Edits::new();
    assert_eq!(edits.sequence("a.txt", "ABABA"), Some(4));
    for reason in [
        RecoveryTransition::AutomaticContinue,
        RecoveryTransition::DirectedContinue,
    ] {
        edits.engine.transition(reason);
        assert!(edits.refused("edit", "a.txt", "B").is_some());
        assert!(edits.refused("write", "a.txt", "final").is_none());
    }
}

#[test]
fn no_recovery_transition_releases_an_uncertain_effect_barrier() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let call = ToolCall {
        id: "uncertain".into(),
        name: "bash".into(),
        arguments: serde_json::json!({"command":"update"}),
        malformed_arguments: false,
    };
    for reason in [
        RecoveryTransition::Steering,
        RecoveryTransition::Coordination,
        RecoveryTransition::AutomaticContinue,
        RecoveryTransition::DirectedContinue,
    ] {
        let mut engine = Engine::default();
        let mut output = ToolOutput::err("lost execution").with_effects(Effects::Unknown);
        engine.settle(&workspace, &call, &mut output, false, false, |_| false);
        engine.transition(reason);
        assert!(engine.pending_refusal().is_some(), "{reason:?}");
        let report = engine
            .drive(|_| Ok(Tick::Finished(completed_report())))
            .unwrap()
            .unwrap();
        assert_eq!(report.outcome, crate::agent::TurnOutcome::Incomplete);
    }
}

#[test]
fn shared_settlement_delivers_cause_specific_advice_without_changing_execution_facts() {
    use crate::runtime::execution::ToolStatus;
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let call = ToolCall {
        id: "denied".into(),
        name: "bash".into(),
        arguments: serde_json::json!({"command":"cargo test"}),
        malformed_arguments: false,
    };
    let mut engine = Engine::default();
    let mut output = ToolOutput::err("denied by policy")
        .with_status(ToolStatus::Blocked)
        .with_effects(Effects::None);
    let facts = output.execution.clone();
    let (_, _, stop) = engine.settle(&workspace, &call, &mut output, false, false, |_| false);
    assert!(stop.is_none());
    assert_eq!(output.execution, facts);
    assert!(output.is_error);
    assert!(output.text.contains("permission_or_policy"));
    assert!(output.text.starts_with("denied by policy"));
}

#[test]
fn first_failure_blocks_normalized_retries_but_not_corrected_calls_or_diagnostics() {
    let mut edits = Edits::new();
    let call = edits.call(
        "bash",
        serde_json::json!({"command":"cargo test", "timeout": 10}),
    );
    edits.settle(&call, vec![], ToolOutput::err("assertion failed"));
    let retry = edits.call(
        "bash",
        serde_json::json!({"command":"cargo test", "timeout": 60, "__intent":"try again"}),
    );
    assert!(
        edits
            .engine
            .rejection_at(&edits.workspace, &retry)
            .is_some()
    );
    let corrected = edits.call(
        "bash",
        serde_json::json!({"command":"cargo test regression"}),
    );
    assert!(
        edits
            .engine
            .rejection_at(&edits.workspace, &corrected)
            .is_none()
    );
    let diagnostic = edits.call("read", serde_json::json!({"path":"src/main.rs"}));
    assert!(
        edits
            .engine
            .rejection_at(&edits.workspace, &diagnostic)
            .is_none()
    );
    edits.neutral_progress();
    edits.engine.evidence_compacted();
    for reason in [
        RecoveryTransition::AutomaticContinue,
        RecoveryTransition::DirectedContinue,
    ] {
        edits.engine.transition(reason);
        assert!(
            edits
                .engine
                .rejection_at(&edits.workspace, &retry)
                .is_some()
        );
    }
    edits.commit("src.rs", "repaired source");
    assert!(
        edits
            .engine
            .rejection_at(&edits.workspace, &retry)
            .is_none()
    );
}

#[test]
fn failed_calls_are_not_evicted_by_other_failures_and_new_steering_releases_them() {
    let mut edits = Edits::new();
    let first = edits.call(
        "browser",
        serde_json::json!({"action":"run", "script":"assert(false)"}),
    );
    edits.settle(
        &first,
        vec![],
        ToolOutput::err("animation assertion failed"),
    );
    for index in 0..100 {
        let call = edits.call(
            "browser",
            serde_json::json!({"action":"run", "script":index}),
        );
        edits.settle(&call, vec![], ToolOutput::err("different failure"));
    }
    assert!(
        edits
            .engine
            .rejection_at(&edits.workspace, &first)
            .is_some()
    );
    edits.engine.transition(RecoveryTransition::Steering);
    assert!(
        edits
            .engine
            .rejection_at(&edits.workspace, &first)
            .is_none()
    );
}

#[test]
fn successful_background_warning_text_does_not_arm_a_failure_barrier() {
    let mut edits = Edits::new();
    let call = edits.call(
        "browser",
        serde_json::json!({"action":"evaluate", "script":"getAnimation()"}),
    );
    edits.settle(
        &call,
        vec![],
        ToolOutput::ok("[VUE_ROUTER_R0004] No match found for /services".into()),
    );
    assert!(edits.engine.rejection_at(&edits.workspace, &call).is_none());
}

#[test]
fn an_edit_refused_only_for_unread_lines_it_revealed_may_be_reissued() {
    let mut edits = Edits::new();
    let edit = edits.call(
        "edit",
        serde_json::json!({"input":"[a.rs#ABCD]\nPUT 3.=3:\n+x"}),
    );
    let refusal = format!(
        "a.rs#ABCD: lines 3 have not been shown by a read, so nothing was written. {}; check them, then re-issue the edit against [a.rs#ABCD].",
        crate::tools::edit::hashline::UNSEEN_LINES_NOW_READ
    );
    edits.settle(&edit, vec![], ToolOutput::err(refusal));
    assert!(edits.engine.rejection_at(&edits.workspace, &edit).is_none());
    // Any other failure of the same edit still arms the barrier.
    edits.settle(&edit, vec![], ToolOutput::err("anchor mismatch"));
    assert!(edits.engine.rejection_at(&edits.workspace, &edit).is_some());
}

#[test]
fn reading_a_failed_edit_target_releases_only_that_edits_barrier() {
    let mut edits = Edits::new();
    edits.write("a.rs", "one\n");
    edits.write("b.rs", "two\n");
    // A resumed session's tag without restored witnesses: the edit failed before writing.
    let edit = edits.call(
        "edit",
        serde_json::json!({"input":"[a.rs#ABCD]\nPUT 1.=1:\n+uno\n"}),
    );
    let other = edits.call(
        "edit",
        serde_json::json!({"input":"[b.rs#ABCD]\nPUT 1.=1:\n+dos\n"}),
    );
    for call in [&edit, &other] {
        edits.settle(
            call,
            vec![],
            ToolOutput::err("no read of this file is recorded; read its affected lines first"),
        );
    }
    let unrelated = edits.call("read", serde_json::json!({"path": "b.rs:9-12"}));
    let failed_read = edits.call("read", serde_json::json!({"path": "a.rs"}));
    edits.settle(&failed_read, vec![], ToolOutput::err("permission denied"));
    assert!(edits.engine.rejection_at(&edits.workspace, &edit).is_some());
    let read = edits.call("read", serde_json::json!({"paths": ["a.rs:1-1"]}));
    edits.settle(&read, vec![], ToolOutput::ok("1:one".to_owned()));
    assert!(edits.engine.rejection_at(&edits.workspace, &edit).is_none());
    assert!(
        edits
            .engine
            .rejection_at(&edits.workspace, &other)
            .is_some(),
        "a read of another file is not this edit's missing evidence"
    );
    edits.settle(&unrelated, vec![], ToolOutput::ok("[b.rs#ABCD]".to_owned()));
    assert!(
        edits
            .engine
            .rejection_at(&edits.workspace, &other)
            .is_none()
    );
    // The released retry fails again: the barrier re-arms until the next read.
    edits.settle(&edit, vec![], ToolOutput::err("unknown snapshot tag"));
    assert!(edits.engine.rejection_at(&edits.workspace, &edit).is_some());
}

#[test]
fn a_failed_shell_command_may_rerun_after_another_session_changes_the_tree() {
    let mut edits = Edits::new();
    let root = edits.workspace.root().to_path_buf();
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&root)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .unwrap();
        assert!(status.success());
    };
    git(&["init", "-q"]);
    std::fs::write(root.join("lib.rs"), "broken {\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "init"]);
    let build = edits.call("bash", serde_json::json!({"command": "cargo check"}));
    edits.settle(&build, vec![], ToolOutput::err("error: unclosed delimiter"));
    assert!(
        edits
            .engine
            .rejection_at(&edits.workspace, &build)
            .is_some(),
        "an unchanged tree keeps the barrier"
    );
    // Another session repairs the file; this session made no edit of its own.
    std::fs::write(root.join("lib.rs"), "fn ok() {}\n").unwrap();
    assert!(
        edits
            .engine
            .rejection_at(&edits.workspace, &build)
            .is_none()
    );
    // Non-shell failures keep the barrier regardless of the tree.
    let read = edits.call("read", serde_json::json!({"path": "missing.rs"}));
    edits.settle(&read, vec![], ToolOutput::err("missing file"));
    std::fs::write(root.join("lib.rs"), "fn other() {}\n").unwrap();
    assert!(edits.engine.rejection_at(&edits.workspace, &read).is_some());
}

#[test]
fn a_post_commit_failure_does_not_release_the_failed_operation() {
    let mut edits = Edits::new();
    let check = edits.call("bash", serde_json::json!({"command":"cargo test"}));
    edits.settle(&check, vec![], ToolOutput::err("assertion failed"));
    let edit = edits.edit_call("edit", "src.rs", "repaired source");
    edits.write("src.rs", "repaired source");
    edits.settle(
        &edit,
        vec![edits.path("src.rs")],
        ToolOutput::err("formatter failed"),
    );
    assert!(
        edits
            .engine
            .rejection_at(&edits.workspace, &check)
            .is_some()
    );
    assert!(edits.engine.rejection_at(&edits.workspace, &edit).is_some());
}

#[test]
fn nonexecuted_refusals_do_not_poison_an_otherwise_valid_retry() {
    use crate::runtime::execution::ToolStatus;
    for status in [ToolStatus::Blocked, ToolStatus::Skipped] {
        let mut edits = Edits::new();
        let call = edits.call("retry_probe", serde_json::json!({"fixed":false}));
        edits.settle(
            &call,
            vec![],
            ToolOutput::err("schema is available next round")
                .with_status(status)
                .with_effects(Effects::None),
        );
        assert!(
            edits.engine.rejection_at(&edits.workspace, &call).is_none(),
            "{status:?}"
        );
        assert!(edits.engine.failed_calls().is_empty());
    }
}

/// Whether the engine withholds the identical retry of `call` right now.
fn barred(edits: &mut Edits, call: &ToolCall) -> bool {
    edits.engine.rejection_at(&edits.workspace, call).is_some()
}

/// A read of `base`, optionally with a read selector. Path resolution sees only the bare path.
fn read_of(edits: &mut Edits, base: &str, selector: &str) -> ToolCall {
    edits.call(
        "read",
        serde_json::json!({"path": format!("{base}{selector}")}),
    )
}

/// A grant request for `dir`, as the model would issue it.
fn grant_of(edits: &mut Edits, dir: &std::path::Path) -> ToolCall {
    edits.call(
        "request_directory",
        serde_json::json!({"path": dir.display().to_string(), "reason": "read notes"}),
    )
}

#[test]
fn a_granted_directory_releases_the_failed_outside_read_for_an_identical_retry() {
    let outside = tempfile::tempdir().unwrap();
    let base = outside.path().join("notes.md").display().to_string();
    for selector in ["", ":1-2"] {
        let mut edits = Edits::new();
        let error = edits
            .workspace
            .resolve(&base)
            .expect_err("outside the workspace");
        let read = read_of(&mut edits, &base, selector);
        edits.settle(&read, vec![], ToolOutput::err(error));
        assert!(barred(&mut edits, &read), "{selector}");
        assert!(
            !edits.engine.failed_access.is_empty(),
            "access error was not tracked for {selector}"
        );
        let grant = grant_of(&mut edits, outside.path());
        // The root is what authorizes the read; the grant's success text alone is not authority.
        assert!(edits.workspace.add_root(outside.path()).unwrap());
        edits.settle(
            &grant,
            vec![],
            ToolOutput::ok("Added the directory. Retry the original tool.".into()),
        );
        edits
            .workspace
            .resolve(&base)
            .expect("granted directory resolves");
        let retry = read_of(&mut edits, &base, selector);
        let rejection = edits.engine.rejection_at(&edits.workspace, &retry);
        assert!(rejection.is_none(), "{selector}: {rejection:?}");
    }
}

#[test]
fn a_failed_directory_grant_does_not_release_the_outside_read() {
    use crate::runtime::execution::ToolStatus;
    let outside = tempfile::tempdir().unwrap();
    let base = outside.path().join("notes.md").display().to_string();
    let mut edits = Edits::new();
    let error = edits
        .workspace
        .resolve(&base)
        .expect_err("outside the workspace");
    let read = read_of(&mut edits, &base, "");
    edits.settle(&read, vec![], ToolOutput::err(error));
    let grant = grant_of(&mut edits, outside.path());
    edits.settle(
        &grant,
        vec![],
        ToolOutput::err("directory access denied")
            .with_status(ToolStatus::Blocked)
            .with_effects(Effects::None),
    );
    edits
        .workspace
        .resolve(&base)
        .expect_err("no root was attached");
    assert!(barred(&mut edits, &read));
}

#[test]
fn an_ordinary_failed_read_inside_a_granted_directory_stays_barred() {
    let outside = tempfile::tempdir().unwrap();
    let base = outside.path().join("missing.md").display().to_string();
    let mut edits = Edits::new();
    assert!(edits.workspace.add_root(outside.path()).unwrap());
    for selector in ["", ":1-2"] {
        edits
            .workspace
            .resolve(&base)
            .expect("inside the granted root");
        let read = read_of(&mut edits, &base, selector);
        edits.settle(&read, vec![], ToolOutput::err("missing file: missing.md"));
        assert!(barred(&mut edits, &read), "{selector}");
        // A repeated grant of a directory already attached is not new evidence for this read.
        let grant = grant_of(&mut edits, outside.path());
        edits.settle(
            &grant,
            vec![],
            ToolOutput::ok("Added the directory. Retry the original tool.".into()),
        );
        assert!(barred(&mut edits, &read), "{selector}");
    }
}

#[test]
fn a_successful_grant_that_does_not_cover_the_read_is_not_authority() {
    let outside = tempfile::tempdir().unwrap();
    let base = outside.path().join("notes.md").display().to_string();
    let mut edits = Edits::new();
    let error = edits
        .workspace
        .resolve(&base)
        .expect_err("outside the workspace");
    let read = read_of(&mut edits, &base, ":1-2");
    edits.settle(&read, vec![], ToolOutput::err(error));
    let check = edits.call("bash", serde_json::json!({"command": "cargo test"}));
    edits.settle(&check, vec![], ToolOutput::err("assertion failed"));
    // An ok grant with no root attached authorizes nothing.
    let grant = grant_of(&mut edits, outside.path());
    edits.settle(
        &grant,
        vec![],
        ToolOutput::ok("Added the directory. Retry the original tool.".into()),
    );
    edits
        .workspace
        .resolve(&base)
        .expect_err("a successful result alone is not authority");
    assert!(barred(&mut edits, &read));
    // A root for a different directory leaves this read outside and barred.
    let other = tempfile::tempdir().unwrap();
    assert!(edits.workspace.add_root(other.path()).unwrap());
    let other_grant = grant_of(&mut edits, other.path());
    edits.settle(
        &other_grant,
        vec![],
        ToolOutput::ok("Added the directory. Retry the original tool.".into()),
    );
    edits
        .workspace
        .resolve(&base)
        .expect_err("another root does not cover the read");
    assert!(barred(&mut edits, &read));
    assert!(
        barred(&mut edits, &check),
        "an unrelated failure stays barred through grants"
    );
}

/// Primary root plus an ordinary attached root holding protected Dial storage (`private/`).
/// Returns the edits, the protected storage dir and the nested sensitive directory.
fn protected_storage() -> (
    Edits,
    tempfile::TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let outside = tempfile::tempdir().unwrap();
    let private = outside.path().join("private");
    let sessions = private.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let mut edits = Edits::new();
    assert!(edits.workspace.add_root(outside.path()).unwrap());
    edits.workspace = edits
        .workspace
        .clone()
        .with_protected_paths(std::slice::from_ref(&private));
    (edits, outside, private, sessions)
}

fn sensitive_grant_of(edits: &mut Edits, dir: &std::path::Path) -> ToolCall {
    edits.call(
        "request_directory",
        serde_json::json!({"path": dir.display().to_string(), "reason": "session notes", "sensitive": true}),
    )
}

#[test]
fn a_sensitive_grant_admits_the_protected_read_and_releases_its_barrier() {
    let (mut edits, _outside, _private, sessions) = protected_storage();
    let base = sessions.join("notes.md").display().to_string();
    let error = edits
        .workspace
        .resolve(&base)
        .expect_err("protected storage is refused");
    assert!(error.contains("enters protected Dial storage"), "{error}");
    let read = read_of(&mut edits, &base, "");
    edits.settle(&read, vec![], ToolOutput::err(error));
    assert!(barred(&mut edits, &read));
    // The human-approved canonical directory is what the agent grants; the engine only observes
    // the successful result and releases the barrier when the target now resolves.
    let canonical = edits.workspace.directory_request(&sessions, true).unwrap();
    let grant = sensitive_grant_of(&mut edits, &sessions);
    edits
        .workspace
        .grant_sensitive_directory(&canonical)
        .unwrap();
    edits.settle(
        &grant,
        vec![],
        ToolOutput::ok("Added the directory. Sensitive native-file access granted.".into()),
    );
    edits
        .workspace
        .resolve(&base)
        .expect("the sensitive grant admits the target");
    let retry = read_of(&mut edits, &base, "");
    assert!(!barred(&mut edits, &retry));
    // Second rejection checkpoint: the released retry fails again, so the barrier re-arms.
    edits.settle(&retry, vec![], ToolOutput::err("missing file: notes.md"));
    assert!(barred(&mut edits, &retry));
}

#[test]
fn a_normal_root_for_protected_storage_stays_protected_and_keeps_the_barrier() {
    let (mut edits, _outside, _private, sessions) = protected_storage();
    let base = sessions.join("notes.md").display().to_string();
    let error = edits
        .workspace
        .resolve(&base)
        .expect_err("protected storage is refused");
    let read = read_of(&mut edits, &base, "");
    edits.settle(&read, vec![], ToolOutput::err(error));
    // An ordinary (non-sensitive) root attaches the directory but never unlocks protected storage.
    let grant = grant_of(&mut edits, &sessions);
    assert!(edits.workspace.add_root(&sessions).unwrap());
    edits.settle(
        &grant,
        vec![],
        ToolOutput::ok("Added the directory. Normal tool approvals remain.".into()),
    );
    let error = edits
        .workspace
        .resolve(&base)
        .expect_err("an ordinary root does not unlock protected storage");
    assert!(error.contains("enters protected Dial storage"), "{error}");
    assert!(barred(&mut edits, &read));
}

#[test]
fn a_sensitive_request_outside_protected_storage_is_refused_and_releases_nothing() {
    use crate::runtime::execution::ToolStatus;
    let (mut edits, outside, _private, sessions) = protected_storage();
    let base = sessions.join("notes.md").display().to_string();
    let error = edits
        .workspace
        .resolve(&base)
        .expect_err("protected storage is refused");
    let read = read_of(&mut edits, &base, "");
    edits.settle(&read, vec![], ToolOutput::err(error));
    // An ancestor of protected storage and an unrelated sibling are not sensitive targets.
    assert!(
        edits
            .workspace
            .directory_request(outside.path(), true)
            .is_err()
    );
    let sibling = outside.path().join("elsewhere");
    std::fs::create_dir(&sibling).unwrap();
    assert!(edits.workspace.directory_request(&sibling, true).is_err());
    let grant = sensitive_grant_of(&mut edits, &sibling);
    edits.settle(
        &grant,
        vec![],
        ToolOutput::err("sensitive access must name a directory inside protected Dial storage")
            .with_status(ToolStatus::Blocked)
            .with_effects(Effects::None),
    );
    assert!(barred(&mut edits, &read));
    assert!(edits.workspace.resolve(&base).is_err());
}
