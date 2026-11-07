//! Final evidence, persistence and learning run only after backend ownership is restored.

use super::unsettled_report;
use crate::agent::*;

impl Agent<'_> {
    pub(super) fn settle_turn(
        &mut self,
        mut result: Result<TurnReport>,
        checkpoint_turn: Option<u64>,
        capture_start: Option<Option<String>>,
        storage_before: crate::storage::session::SessionIoMetrics,
        ui: &mut dyn Ui,
    ) -> Result<TurnReport> {
        // Contracted nodes are fail-closed even after late hooks, user edits and persistence errors.
        self.sync_contracts()?;
        if let Ok(report) = &mut result
            && report.outcome == TurnOutcome::Completed
            && let Some(reason) = self.workflow_contracts.blocker()
        {
            report.outcome = TurnOutcome::Incomplete;
            report.text = unsettled_report(&report.text, &reason);
            ui.notice(&reason);
        }
        // Hooks and late verification may invalidate an earlier accepted candidate. Strict
        // requirements are checked again at final settlement without launching extra processes.
        if let Ok(report) = &mut result
            && report.outcome == TurnOutcome::Completed
            && let Some(reason) = crate::agent::verify::requirements::strict_blocker(
                &self.config.verification,
                &self.step_checks.evidence,
                self.step_checks.revision(),
                &self.step_checks.visual,
            )
        {
            report.outcome = TurnOutcome::Incomplete;
            report.text = unsettled_report(&report.text, &reason);
            ui.notice(&reason);
        }
        // Publish only after all hook continuations settle, not for an intermediate candidate.
        if !self.hermetic
            && !(self.config.verification.enabled && self.config.verification.automatic_checks)
            && result
                .as_ref()
                .is_ok_and(|report| report.outcome == TurnOutcome::Completed)
        {
            self.publish_no_automatic_check(ui);
        }
        let completion_evidence = result
            .as_ref()
            .ok()
            .map(|report| self.completion_evidence(report.outcome));
        if let Some(evidence) = &completion_evidence {
            let remaining = evidence["outstanding_tasks"].as_array().map_or(0, Vec::len);
            if remaining > 0 {
                let notice = format!(
                    "{remaining} progress item(s) remain open; this report does not establish their completion."
                );
                ui.notice(&notice);
                if let Ok(report) = &mut result
                    && report.outcome == TurnOutcome::Completed
                {
                    report.text.push_str(&format!("\n\n{notice}"));
                }
            }
            self.record(0, "completion_evidence", evidence.clone())?;
        }
        self.ext.fire_lifecycle(&LifecycleEvent {
            event: "turn_finished".into(),
            session_id: Some(self.session.id().to_owned()),
            turn_id: checkpoint_turn.map(|turn| turn.to_string()),
            tool_name: None,
            is_error: Some(!result.as_ref().is_ok_and(|report| {
                report.outcome == TurnOutcome::Completed && !report.interrupted
            })),
        });
        self.session.repair_incomplete_tools()?;
        if let Ok(report) = &result {
            self.efficiency_metrics.delivered_answer_bytes += report.text.len() as u64;
            let mut state = self.session.control_state();
            state["turn_outcome"] = json!(report.outcome);
            state["completion_evidence"] = completion_evidence.clone().unwrap_or(Value::Null);
            self.session.set_control_state(state)?;
        }
        if let Some(plan) = self.pending_plan.take() {
            self.push(json!({"role":"user","content":format!("Approved plan:\n{plan}\nProceed with implementation.")}))?;
        }
        if result
            .as_ref()
            .is_ok_and(|report| report.outcome == TurnOutcome::Completed)
        {
            self.finish_experience(ui);
            self.save_investigation_findings(ui);
            // The default architecture model is local MLX. With a cloud main model no compute is held for
            // the turn, so synthesis is skipped rather than making every turn wait for training.
            let arch_local = self.config.architecture_config().runs_dial_mlx();
            if !self.hermetic
                && self.config.codebase.synthesize_architecture
                && (!arch_local || self.config.uses_local_compute())
                && self.config.architecture_config().context_window >= 4096
                && !self.interrupt.load(Ordering::SeqCst)
                && ui.maintenance_ready()
                && let Err(error) = self.synthesize_architecture(false, ui)
            {
                ui.diagnostic(&format!("Architecture update deferred: {error:#}"));
            }
        }
        if let (Ok(report), Some(predecessor)) = (&result, capture_start) {
            self.schedule_memory_capture(report, predecessor.as_deref(), ui);
        }
        let (_, usage) = self.usage_scope.snapshot();
        let efficiency = json!({
            "version":1,
            "task_kind":self.task_policy.kind,
            "enabled":self.config.efficiency.enabled,
            "completed":result.as_ref().is_ok_and(|r| r.outcome == TurnOutcome::Completed),
            "outcome":result.as_ref().map(|r| r.outcome).ok(),
            "duration_ms":self.workflow_started.elapsed().as_millis(),
            "inference_latency":ui.request_latency(),
            "usage":usage,
            "completion_evidence":completion_evidence,
            "projection_bytes":self.efficiency_metrics,
            "scheduling":self.efficiency_metrics.scheduling,
            "worker_scheduling":self.efficiency_metrics.worker_scheduling,
            "storage_io":self.session.io_metrics().since(&storage_before),
            "note":"Usage is provider-reported and may be partial; cache semantics vary by protocol. Projection bytes are not billed tokens. Completion is not independent correctness proof."
        });
        let mut control = self.session.control_state();
        control["efficiency"] = efficiency.clone();
        self.session.set_control_state(control)?;
        ui.efficiency(&efficiency);
        self.finish_turn_trajectory(result, completion_evidence.as_ref(), ui)
    }

    /// Retain observations and separately gated behavioral labels, never model-assigned rewards.
    fn finish_turn_trajectory(
        &mut self,
        result: Result<TurnReport>,
        completion_evidence: Option<&Value>,
        ui: &mut dyn Ui,
    ) -> Result<TurnReport> {
        if let Some((id, _, _)) = self.trajectory.take() {
            if let Some(store) = self.store {
                match &result {
                    Ok(report) => {
                        store.finish_trajectory(id, Some(&report.text), None)?;
                        if report.outcome == TurnOutcome::Completed
                            && self.domain_override.is_none()
                            && !self.hermetic
                            && self
                                .step_checks
                                .evidence
                                .passing_behavioral(self.step_checks.revision())
                                > 0
                            && self.step_checks.clean()
                            && completion_evidence.is_some_and(|e| {
                                e["outstanding_tasks"].as_array().is_some_and(Vec::is_empty)
                                    && e["visual_evidence"] != "missing"
                                    && e["requirements"]["coverage"] != "requirements_uncovered"
                            })
                            && let Err(error) = store.outcome(&crate::config::types::Outcome {
                                trajectory: id,
                                score: 100.0,
                                source: "checks:behavioral".into(),
                                reference: Some(format!(
                                    "{} behavioral check(s) passed after the last edit",
                                    self.step_checks
                                        .evidence
                                        .passing_behavioral(self.step_checks.revision())
                                )),
                            })
                        {
                            ui.diagnostic(&format!("turn outcome was not recorded: {error:#}"));
                        }
                        if report.outcome == TurnOutcome::Completed
                            && let Err(error) = self.persist_turn(store, id)
                        {
                            ui.diagnostic(&format!(
                                "training observation was not saved: {error:#}"
                            ));
                        }
                    }
                    Err(error) => store.finish_trajectory(id, None, Some(&format!("{error:#}")))?,
                }
            }
            if let Ok(report) = &result {
                return Ok(TurnReport {
                    trajectory: Some(id),
                    ..result_clone(report)
                });
            }
        }
        result
    }
}
