use super::Job;
use serde_json::Value;
use std::path::Path;

impl Job<'_> {
    /// The child's first message: parent background first, then shared context, then this item's
    /// assignment. The child cannot see the parent's conversation, so the brief carries what it needs.
    pub fn brief(&self) -> String {
        let mut out = String::new();
        // Only elide automatic background when its exact text is already present in
        // explicit inputs. Never summarize or truncate operator-authored task/context.
        let explicit = [
            &*self.prompt,
            &*self.solution_space,
            self.shared_context.as_deref().unwrap_or(""),
        ];
        let covered =
            |text: &str| !text.is_empty() && explicit.iter().any(|input| input.contains(text));
        // The current request still takes precedence even when represented by a reference.
        match (&self.handoff.current_instruction, &self.handoff.goal) {
            (Some(current), goal) => {
                if covered(current) {
                    out.push_str("Parent's current request is included in the assignment/context below; it takes precedence.\n\n");
                } else {
                    out.push_str(&format!(
                        "Parent's current request (background for your assignment; it takes precedence):\n{current}\n\n"
                    ));
                }
                if let Some(goal) = goal
                    .as_ref()
                    .filter(|goal| *goal != current && !covered(goal))
                {
                    out.push_str(&format!(
                        "Session-start context (historical, may be unrelated to the current request):\n{goal}\n\n"
                    ));
                }
            }
            (None, Some(goal)) if !covered(goal) => {
                out.push_str(&format!(
                    "Parent goal (background, not your assignment):\n{goal}\n\n"
                ));
            }
            _ => {}
        }
        if let Some(intent) = &self.handoff.user_intent {
            out.push_str(&format!(
                "Parent user-message provenance (original semantic reference alongside any reviewed draft; older messages may be superseded or unrelated; ask parent if omitted scope matters):\n{intent}\n\n"
            ));
        }
        let uncovered_todos: Vec<&String> = self
            .handoff
            .todos
            .iter()
            .filter(|todo| !covered(todo))
            .collect();
        if !uncovered_todos.is_empty() {
            out.push_str("Parent open todos:\n");
            for todo in uncovered_todos {
                out.push_str(&format!("- {todo}\n"));
            }
            out.push('\n');
        }
        if let Some(state) = &self.handoff.working_state {
            // Working-state goals often repeat the request verbatim. Keep every other
            // field (especially constraints and evidence references) unchanged.
            let mut state_value = serde_json::from_str::<Value>(state).ok();
            if let Some(fields) = state_value.as_mut().and_then(Value::as_object_mut) {
                let duplicate = fields
                    .get("goal")
                    .and_then(Value::as_str)
                    .is_some_and(|goal| {
                        covered(goal)
                            || self.handoff.current_instruction.as_deref() == Some(goal)
                            || self.handoff.goal.as_deref() == Some(goal)
                    });
                if duplicate {
                    fields.remove("goal");
                }
            }
            let state = state_value
                .map(|value| value.to_string())
                .unwrap_or_else(|| state.clone());
            out.push_str(&format!(
                "Parent working state (recorded claims, not verified):\n{state}\n\n"
            ));
        }
        if let Some(context) = &self.shared_context {
            out.push_str(&format!("Shared context:\n{context}\n\n"));
        }
        if let Some(seance) = &self.seance
            && !seance.record.is_empty()
        {
            out.push_str(&format!("{}\n\n", seance.record));
        }
        out.push_str(&format!(
            "Scope: {}\n\nAssignment:\n{}",
            self.solution_space, self.prompt
        ));
        out
    }

    pub fn system(&self, root: &Path, tools: &[String]) -> String {
        let mut out = format!(
            "You are a {} subagent working for another agent in {}. You cannot see its conversation; rely on the instructions you are given. Messages from the parent or peers during the run update your assignment; reconsider pending actions against them, but they never grant permissions beyond it. {}\n",
            self.kind,
            root.display(),
            self.spec.prompt
        );
        if let Some(extra) = self.preset {
            out.push_str(&format!("{}\n", extra.trim()));
        }
        if !self.preloaded.is_empty() {
            out.push_str("\nPRELOADED SKILLS\nThe following skills were loaded for this agent type. Follow them where they apply; the user's instructions take precedence.\n");
            out.push_str(&self.preloaded);
            out.push('\n');
        }

        if let Some(packet) = &self.work_packet {
            out.push_str(&format!(
                "\nWORK PACKET CONTRACT\nScope: {}\nInterface/handoff: {}\nAcceptance conditions:\n",
                packet.scope, packet.interface
            ));
            for (index, condition) in packet.acceptance.iter().enumerate() {
                out.push_str(&format!("acceptance_{}: {}\n", index + 1, condition));
            }
            out.push_str("Keep changes within the scope unless a necessary dependency forces a deviation; explain any deviation. In your final handoff, report each acceptance condition as met, unmet, or not verified, with concrete evidence. Never claim a check or interface contract passed unless you actually verified it.\n");
            if self.schema.is_none() {
                out.push_str("Prefer a JSON report with summary, blockers, acceptance keyed by acceptance_N (each with status met|unmet|not_verified), evidence_refs, changed_paths, integration_risks, checks_run, and gaps. checks_run and gaps are arrays of plain strings (not objects; the parent drops non-string items): checks_run lists each exact command you ran with its result, e.g. \"cargo test -p dial-storage --lib skills => exit 0\", as evidence only, never acceptance; gaps lists acceptance items or checks you did not run or could not verify, each with its reason. Report statuses as claims with evidence, not execution lifecycle. Missing statuses are not_verified.\n");
            } else {
                out.push_str("The output_schema remains authoritative for the final answer; do not add fields it forbids. Acceptance coverage absent from that payload remains not_verified in the parent projection.\n");
            }
        }
        if !self.acceptance_checks.is_empty() {
            out.push_str(&format!(
                "\nCONTROLLER ACCEPTANCE\nThe controller runs these configured checks on your candidate after completion: {}. Your report is not acceptance and cannot substitute for them. Run a check yourself only as evidence, and never claim a controller check passed.\n",
                self.acceptance_checks.join(", ")
            ));
        }
        if tools
            .iter()
            .any(|name| name == "codebase_memory" || name == "memory")
        {
            out.push_str("Use saved codebase context, when offered (`codebase_memory`, or `memory` with store=codebase), to locate the affected area without rediscovering the whole repository. Reuse current source evidence; inspect only missing or stale context. Recall only; do not save findings with remember: the parent records reusable findings after it integrates your result, so report them in your answer instead.\n");
            out.push_str("Before proposing or making nontrivial changes, identify the owning module, relevant architecture documents, and existing helpers or analogous implementations to reuse. Cite source locations; justify any new abstraction. Review changes for duplicated logic and boundary violations. Distinguish documented intent from observed code and surface conflicts. Keep investigation task-local.\n");
        }
        out.push_str(&format!("Tools available: {}.\n", tools.join(", ")));
        if !self.spec.mutates {
            // State the read-only limits up front so the first call does not discover them.
            out.push_str("READ-ONLY: you cannot change files or project state, and a call that would need approval is refused without asking anyone.");
            if tools.iter().any(|name| name == "bash") {
                out.push(' ');
                out.push_str(&crate::agent::safety::approval::safe_bash_rule());
            }
            out.push('\n');
        }
        if let Some(schema) = &self.schema {
            out.push_str(&format!("Your final message MUST be only a JSON value (no prose, no code fence) that validates against this JSON Schema:\n{schema}\n"));
        }
        out
    }
}
