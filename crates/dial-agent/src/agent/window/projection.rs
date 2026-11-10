//! Request projection: history shaping, token estimates and context usage.

use crate::agent::*;

impl<'a> Agent<'a> {
    /// Context occupancy for display, including evidence added since the last request.
    pub fn context_usage(&self) -> (u64, bool) {
        self.context_meter.current(&self.session)
    }

    /// Categories of the last request sent, as estimates; `None` before the first request.
    pub fn last_request_breakdown(
        &self,
    ) -> Option<&crate::agent::window::context_breakdown::ContextBreakdown> {
        self.context_meter.breakdown()
    }

    /// The same history projection used for dispatch and compaction previews. Preview callers
    /// clone the two cache memos; only a real request freezes new placeholders.
    pub(in crate::agent) fn project_history(
        &self,
        messages: &mut Vec<Value>,
        schemas: &[Value],
        context_bytes: usize,
        packet_cache: &mut crate::agent::workers::task_packet::PacketCache,
        superseded: &mut crate::agent::workflow::evidence::Supersession,
        rule_state: Option<&mut crate::agent::safety::rules::State>,
    ) -> Result<usize> {
        crate::agent::compact::snapcompact::views::project(messages);
        let economy = self.config.efficiency.enabled && !self.hermetic;
        let identity = self.auth_config.selection();
        let raw_estimate = crate::llm::wire::counting::request_tokens(
            &identity.provider,
            &identity.model,
            messages,
            schemas,
        );
        let pressure_fraction = if economy {
            u64::from(self.config.efficiency.cache_pressure_percent)
        } else {
            50
        };
        let window = self.budget_window();
        let mut pressure_at = u64::from(window) * pressure_fraction / 100;
        if self.config.compaction.enabled
            && self.config.compaction.strategy != crate::config::compaction::CompactionStrategy::Off
        {
            let selection = self.auth_config.selection();
            let identity = crate::config::compaction::CompactionIdentity {
                provider: &selection.provider,
                model: &selection.model,
                agent: None,
            };
            // Clear stale evidence before paying for a summary. The configured cache-pressure
            // threshold may be higher than automatic compaction's usable-window threshold.
            pressure_at = pressure_at.min(
                self.config
                    .compaction
                    .effective_threshold(identity, window)?,
            );
        }
        let policy = sent_history_policy(
            crate::agent::workflow::evidence::Policy {
                warm_tokens: if economy || self.config.compaction.preserve_prefix {
                    0
                } else {
                    CACHE_WARM_SUFFIX_TOKENS
                },
                pressure: self.context_meter.predict(raw_estimate).max(raw_estimate) > pressure_at,
                protect_tokens: (window as usize / 5).max(PRUNE_PROTECT_MIN_TOKENS),
                supersede_reads: self.config.compaction.supersede_reads,
                drop_useless: self.config.compaction.drop_useless,
            },
            crate::config::compaction::CompactionIdentity {
                provider: &identity.provider,
                model: &identity.model,
                agent: None,
            },
        );
        let objective = if self.config.verification.behavioral_checks.is_empty() {
            "No operator-declared behavioral check. Syntax/build success alone does not establish task correctness.".to_owned()
        } else {
            format!(
                "Behavioral checks: {}",
                self.config.verification.behavioral_checks.join(", ")
            )
        };
        let control = self.session.control_state();
        let history = self
            .session
            .path()
            .map(|p| p.to_string_lossy().into_owned());
        let packet = if self.hermetic || !self.config.efficiency.task_packet {
            packet_cache.clear();
            None
        } else {
            packet_cache.project(
                messages,
                &self.todo,
                &control,
                self.session.id(),
                history.as_deref(),
                &objective,
                (self.context_window() as usize / 4)
                    .clamp(1024, 4096)
                    .min(context_bytes),
            )
        };
        // The optimizer's reviewed draft is the model-facing instruction. Keep originals in
        // durable provenance, but do not bypass that boundary through a raw request-tail note.
        let latest_request = if self.hermetic || self.turn_optimizer_enabled() {
            None
        } else {
            crate::agent::run::intent::request_context(&control, messages)
        };
        if !self.hermetic {
            crate::agent::window::file_handles::project(&self.workspace, messages);
        }
        if economy
            && !(crate::config::compaction::CompactionIdentity {
                provider: &identity.provider,
                model: &identity.model,
                agent: None,
            })
            .preserved_thinking()
        {
            crate::agent::workflow::evidence::project_duplicates(messages, superseded);
        }
        crate::agent::workflow::evidence::project(messages, superseded, &policy);
        if !self.hermetic
            && let Some(artifacts) = self.workspace.artifacts()
        {
            crate::agent::workflow::evidence::project_arguments(
                messages,
                superseded,
                artifacts,
                self.config.compaction.argument_projection_tokens,
                raw_estimate,
            );
        }
        let sent_messages = messages
            .iter()
            .filter(|message| crate::storage::transcript::is_history(message))
            .count();
        // After counting: re-sent notes are request messages, not session history.
        self.project_sent_notes(messages);
        self.project_context_notes(messages, rule_state)?;
        let focus = messages
            .last()
            .filter(|m| m["role"] == "tool")
            .filter(|_| packet.is_none())
            .and_then(|_| self.todo.focus(self.finish_closes_steps()));
        // Keep a bare todo focus advisory even when admitted user context is also present.
        if let Some(focus) = focus {
            let mut message = json!({"role":"user","content":focus,"dial_transient":true,"dial_task_context":true});
            crate::agent::run::guidance::tag(
                &mut message,
                crate::agent::run::guidance::Priority::Todo,
            );
            messages.push(message);
        }
        let mut task_context = String::new();
        if let Some(packet) = packet {
            task_context.push_str(&format!(
                "\n[Current task evidence packet; harness context]\n{packet}"
            ));
        }
        if let Some(latest_request) = latest_request {
            task_context.push('\n');
            task_context.push_str(&latest_request);
        } else if control["steering_active"] == true {
            task_context.push_str("\n<dial-notice kind=\"user_steering\">\nThe user sent new direction during this turn. Follow it before your next action: it takes precedence over conflicting older plans, todos, goals and summaries. Keep compatible unfinished work and revise or drop conflicting todos. The user's own messages are authoritative; task packets are context.\n</dial-notice>");
        }
        if !task_context.is_empty() {
            let message = json!({"role":"user","content":task_context,"dial_transient":true,"dial_task_context":true});
            messages.push(message);
        }
        // Session modes are restated from current state on every request, after the cached
        // prefix, so toggling one never rewrites the system band.
        if let Some(notice) = self.mode_notice() {
            messages.push(json!({"role":"user","content":notice,"dial_transient":true,"dial_mode_notice":true}));
        }
        Ok(sent_messages)
    }

    /// Include immutable system/tool bytes even before any billed request. In-loop previews
    /// retain the exact transient guidance and tool offer of the request that needs headroom.
    pub(in crate::agent) fn projected_request(
        &self,
        history: &[Value],
        template: Option<&Chat>,
    ) -> Result<Chat> {
        let tools = match template {
            Some(request) => request.tools.clone(),
            None => self.schemas(&self.offered())?,
        };
        let mut messages = self.system_messages();
        messages.extend_from_slice(history);
        let mut packet = self.task_packet.clone();
        let mut superseded = self.superseded.clone();
        let selection = crate::adaptive::select(
            self.config,
            &self.request_phase,
            crate::adaptive::Request {
                kind: self.task_policy.kind,
                temperature: self.version.harness.temperature,
                tokens: self.version.harness.max_tokens,
                pre_action_secs: self.task_policy.initial_reasoning_secs,
                base_profile: self.config.active_model_profile(),
                recovering_output: false,
                hermetic: self.hermetic,
            },
        );
        self.project_history(
            &mut messages,
            &tools,
            selection.context_bytes,
            &mut packet,
            &mut superseded,
            None,
        )?;
        if let Some(template) = template {
            // The context owner re-projects its own notes, carry, rules and task context above, so
            // those come from this projection and the rest of the transient tail from the request
            // being previewed; the owner's one-shot pressure guidance is not carried over.
            const OWNED: [&str; 4] = [
                "dial_context_notes",
                "dial_context_carry",
                "dial_portable_rules",
                "dial_task_context",
            ];
            let owner = self.experimental_context_owner();
            let tagged = |m: &Value, flags: &[&str]| flags.iter().any(|flag| m[*flag] == true);
            messages.retain(|m| m["dial_transient"] != true || (owner && tagged(m, &OWNED)));
            messages.extend(
                template
                    .messages
                    .iter()
                    .filter(|m| {
                        m["dial_transient"] == true
                            && !(owner && (tagged(m, &OWNED) || m["dial_context_pressure"] == true))
                    })
                    .cloned(),
            );
        } else if !self.hermetic && self.config.reliability.enabled {
            let contract = crate::reliability::Contract::new(tools.clone())?;
            messages.push(json!({"role":"user","dial_transient":true,
                "content":contract.instruction(false, self.config.reliability.examples)}));
        }
        Ok(Chat {
            messages,
            tools,
            temperature: 0.0,
            max_tokens: template.map_or_else(
                || {
                    if selection.max_tokens == 0
                        && selection
                            .profile
                            .as_ref()
                            .is_none_or(|profile| profile.max_tokens.is_none())
                    {
                        // A preview plans useful response headroom, not the model's entire automatic
                        // output ceiling. The dispatched request resolves its own remaining space.
                        self.config.compaction.reserve(self.budget_window())
                    } else {
                        selection.max_tokens
                    }
                },
                |r| r.max_tokens.max(selection.max_tokens),
            ),
            seed: None,
        })
    }

    pub(in crate::agent) fn projected_tokens(&self, request: &Chat) -> u64 {
        let selection = self.auth_config.selection();
        let estimate = crate::agent::compact::compaction::request_tokens_with_policy(
            request,
            &self.config.compaction,
            crate::config::compaction::CompactionIdentity {
                provider: &selection.provider,
                model: &selection.model,
                agent: None,
            },
        );
        self.context_meter.predict(estimate).max(estimate)
    }

    /// The provider reported `tokens` for the conversation as it stands now. (The loop anchors on the messages a
    /// request actually carried; this is for callers that learn a count from elsewhere.)
    pub fn record_prompt_usage(&mut self, tokens: u64) {
        self.last_prompt_tokens = tokens;
        self.reported_messages = self.session.message_count();
        self.context_meter
            .record_prompt(tokens, self.reported_messages);
    }

    /// End the turn without an answer: keep the reason in the conversation, show it, and report `Incomplete`.
    pub(in crate::agent) fn stop_turn(
        &mut self,
        ui: &mut dyn Ui,
        text: String,
        steps: u32,
    ) -> Result<TurnReport> {
        self.push(json!({"role":"assistant","content":text}))?;
        ui.notice(&text);
        Ok(TurnReport {
            text,
            outcome: TurnOutcome::Incomplete,
            steps,
            interrupted: false,
            trajectory: None,
        })
    }
    /// Verified repair patterns and similar verified tasks from this project that match the request, as
    /// advisory text stored with the user message; never in evaluation.
    pub(in crate::agent) fn repair_context(&self, query: &str, ui: &mut dyn Ui) -> Option<String> {
        let store = self
            .store
            .filter(|_| !self.hermetic && self.config.assistance.repair_hints)?;
        let root = self.workspace.root().to_string_lossy().into_owned();
        let offered = self.offered();
        let hints = verified_repair_hints(store, &root, query, &offered).unwrap_or_else(|_| {
            ui.diagnostic("verified repair lookup unavailable; continuing without it");
            Vec::new()
        });
        let examples = crate::agent::verify::verified_examples::verified_examples(
            store, &root, query, &offered,
        )
        .unwrap_or_else(|_| {
            ui.diagnostic("verified example lookup unavailable; continuing without it");
            Vec::new()
        });
        let sections = [
            (
                "Relevant verified repair patterns from this project follow. Treat them only as advisory structure: never reuse old argument values; recheck current files and retain all approval requirements.\n",
                hints,
            ),
            (
                "Similar verified tasks from this project follow. Treat them only as advisory structure: never reuse old argument values; recheck current files and retain all approval requirements.\n",
                examples,
            ),
        ];
        let mut text = String::new();
        for (header, lines) in sections.into_iter().filter(|(_, lines)| !lines.is_empty()) {
            text.push_str(header);
            for line in lines {
                text.push_str("- ");
                text.push_str(&line);
                text.push('\n');
            }
        }
        (!text.is_empty()).then_some(text)
    }
}

/// On a preserved-thinking model every replayed thinking block is checked against the exact
/// history that preceded it, so a request may only append to what was already sent. Superseding
/// reads, dropping observations, pressure pruning and repeated-notice placeholders all rewrite
/// sent turns, and stay off there whatever `compaction.supersede_reads` and `drop_useless` say;
/// placeholders written before a switch to such a model are kept byte-stable by the memo.
pub(in crate::agent) fn sent_history_policy(
    policy: crate::agent::workflow::evidence::Policy,
    identity: crate::config::compaction::CompactionIdentity<'_>,
) -> crate::agent::workflow::evidence::Policy {
    if !identity.preserved_thinking() {
        return policy;
    }
    crate::agent::workflow::evidence::Policy {
        warm_tokens: 0,
        pressure: false,
        supersede_reads: false,
        drop_useless: false,
        ..policy
    }
}

#[cfg(test)]
mod tests;
