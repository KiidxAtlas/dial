//! Request projection: history shaping, token estimates and context usage.

use super::*;

impl<'a> Agent<'a> {
    /// Context occupancy for display, including evidence added since the last request.
    pub fn context_usage(&self) -> (u64, bool) {
        self.context_meter.current(&self.session)
    }

    /// The same history projection used for dispatch and compaction previews. Preview callers
    /// clone the two cache memos; only a real request freezes new placeholders.
    pub(super) fn project_history(
        &self,
        messages: &mut Vec<Value>,
        schemas: &[Value],
        context_bytes: usize,
        packet_cache: &mut task_packet::PacketCache,
        superseded: &mut evidence::Supersession,
        rule_state: Option<&mut rules::State>,
    ) -> Result<usize> {
        let economy = self.config.efficiency.enabled && !self.hermetic;
        let raw_estimate = crate::llm::wire::estimate_parts(messages, schemas);
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
        let policy = evidence::Policy {
            warm_tokens: if economy || self.config.compaction.preserve_prefix {
                0
            } else {
                CACHE_WARM_SUFFIX_TOKENS
            },
            pressure: self.context_meter.predict(raw_estimate).max(raw_estimate) > pressure_at,
            protect_tokens: (window as usize / 5).max(PRUNE_PROTECT_MIN_TOKENS),
            supersede_reads: self.config.compaction.supersede_reads,
            drop_useless: self.config.compaction.drop_useless,
        };
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
        evidence::project(messages, superseded, &policy);
        let sent_messages = messages
            .iter()
            .filter(|message| crate::storage::transcript::is_history(message))
            .count();
        self.project_context_notes(messages, rule_state)?;
        let focus = messages
            .last()
            .filter(|m| m["role"] == "tool")
            .filter(|_| packet.is_none())
            .and_then(|_| self.todo.focus());
        let mut task_context = focus.unwrap_or_default();
        if let Some(packet) = packet {
            task_context.push_str(&format!(
                "\n[Current task evidence packet; harness context]\n{packet}"
            ));
        }
        if control["steering_active"] == true {
            task_context.push_str("\n[User steering from Dial] Follow the latest user instruction before taking another action. New user corrections take precedence over conflicting older plans, todos, original goals and summaries. Keep compatible unfinished work, but revise or drop conflicting todos instead of continuing the old task. The user's full messages are authoritative; task packets are context, not permission to ignore them.");
        }
        if !task_context.is_empty() {
            messages.push(json!({"role":"user","content":task_context,"dial_transient":true,"dial_task_context":true}));
        }
        Ok(sent_messages)
    }

    /// Include immutable system/tool bytes even before any billed request. In-loop previews
    /// retain the exact transient guidance and tool offer of the request that needs headroom.
    pub(super) fn projected_request(
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
            messages.retain(|m| {
                m["dial_transient"] != true
                    || (self.experimental_context_owner()
                        && (m["dial_context_notes"] == true
                            || m["dial_context_carry"] == true
                            || m["dial_portable_rules"] == true
                            || m["dial_task_context"] == true))
            });
            messages.extend(
                template
                    .messages
                    .iter()
                    .filter(|m| {
                        m["dial_transient"] == true
                            && (!self.experimental_context_owner()
                                || (m["dial_context_notes"] != true
                                    && m["dial_context_carry"] != true
                                    && m["dial_portable_rules"] != true
                                    && m["dial_task_context"] != true
                                    && m["dial_context_pressure"] != true))
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
            max_tokens: template.map_or(selection.max_tokens, |r| {
                r.max_tokens.max(selection.max_tokens)
            }),
            seed: None,
        })
    }

    pub(super) fn projected_tokens(&self, request: &Chat) -> u64 {
        let selection = self.auth_config.selection();
        let estimate = compaction::request_tokens_with_policy(
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
    pub(super) fn stop_turn(
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
    pub(super) fn repair_context(&self, query: &str, ui: &mut dyn Ui) -> Option<String> {
        let store = self
            .store
            .filter(|_| !self.hermetic && self.config.assistance.repair_hints)?;
        let root = self.workspace.root().to_string_lossy().into_owned();
        let offered = self.offered();
        let hints = verified_repair_hints(store, &root, query, &offered).unwrap_or_else(|_| {
            ui.diagnostic("verified repair lookup unavailable; continuing without it");
            Vec::new()
        });
        let examples = verified_examples::verified_examples(store, &root, query, &offered)
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
