use super::*;
impl Terminal for () {
    fn prevent_completion(&mut self, _: &str) {}
}
impl Terminal for u32 {
    fn prevent_completion(&mut self, _: &str) {}
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
fn reset_from_new_steering_clears_turn_local_cycle_state() {
    let mut edits = Edits::new();
    assert_eq!(edits.sequence("a.txt", "ABABA"), Some(4));
    edits.engine.reset();
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
