use serde_json::Value;

/// Worker result payload; its state is the shared execution lifecycle.
pub(in crate::agent) struct Status {
    pub phase: crate::runtime::Phase,
    pub text: String,
}
impl Status {
    pub fn done(text: String) -> Self {
        Self {
            phase: crate::runtime::Phase::Completed,
            text,
        }
    }
    pub fn failed(text: String) -> Self {
        Self {
            phase: crate::runtime::Phase::Failed,
            text,
        }
    }
    pub fn incomplete(text: String) -> Self {
        Self {
            phase: crate::runtime::Phase::Incomplete,
            text,
        }
    }
    pub fn interrupted() -> Self {
        Self {
            phase: crate::runtime::Phase::Interrupted,
            text: "interrupted".into(),
        }
    }
    /// Keep the established task-tool wire spelling; runtime nodes use canonical phases.
    pub fn wire(&self) -> &'static str {
        match self.phase {
            crate::runtime::Phase::Completed => "done",
            crate::runtime::Phase::NeedsInput => "needs_input",
            state => state.label(),
        }
    }
}

pub(in crate::agent) struct Outcome {
    pub status: Status,
    pub data: Option<Value>,
    /// Execution lifecycle, never mixed into an output-schema payload.
    pub isolation: Option<crate::agent::workers::isolation::Merge>,
    pub acceptance: crate::agent::verify::worker_acceptance::Acceptance,
    /// Non-executing guidance identities/reminders, separate from validated child output.
    pub portable_rules: Option<Value>,
    /// Model requests made.
    pub steps: u32,
    pub retryable_provider: bool,
    pub model_route: Option<Value>,
    /// Controller-owned exact inspection identities; never part of the worker's validated answer.
    pub inspections: crate::agent::workflow::efficiency::Inspections,
    pub scheduling: crate::agent::workflow::efficiency::Scheduling,
    /// The child's conversation at finish, moved out so a retained slot can continue it.
    pub conversation: Option<ChildConversation>,
}
/// A finished child's validated history and the tool names it was run with.
pub(in crate::agent) struct ChildConversation {
    pub messages: Vec<Value>,
    pub tools: Vec<String>,
}
impl crate::agent::run::engine::Terminal for Outcome {
    fn prevent_completion(&mut self, reason: &str) {
        if self.status.phase == crate::runtime::Phase::Completed {
            self.status.phase = crate::runtime::Phase::Incomplete;
            self.status.text.push_str(&format!("\n{reason}"));
        }
    }
}

impl Outcome {
    pub fn incomplete(steps: u32, why: impl Into<String>) -> Self {
        Self {
            status: Status::incomplete(why.into()),
            data: None,
            isolation: None,
            acceptance: Default::default(),
            portable_rules: None,
            steps,
            retryable_provider: false,
            model_route: None,
            inspections: Default::default(),
            scheduling: Default::default(),
            conversation: None,
        }
    }
    pub fn failed(steps: u32, why: impl Into<String>) -> Self {
        Self {
            status: Status::failed(why.into()),
            data: None,
            isolation: None,
            acceptance: Default::default(),
            portable_rules: None,
            steps,
            retryable_provider: false,
            model_route: None,
            inspections: Default::default(),
            scheduling: Default::default(),
            conversation: None,
        }
    }
    pub fn interrupted(steps: u32) -> Self {
        Self {
            status: Status::interrupted(),
            data: None,
            isolation: None,
            acceptance: Default::default(),
            portable_rules: None,
            steps,
            retryable_provider: false,
            model_route: None,
            inspections: Default::default(),
            scheduling: Default::default(),
            conversation: None,
        }
    }
    pub fn provider_failed(
        steps: u32,
        error: &anyhow::Error,
        fallback_on: crate::config::types::FallbackOn,
    ) -> Self {
        let mut outcome = Self::failed(
            steps,
            crate::agent::run::failure_guidance::provider_failure(error),
        );
        outcome.retryable_provider = crate::llm::fallback::should_fallback(error, fallback_on);
        outcome
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::agent) enum MutationScope {
    Workspace,
    ManagedMemory,
    SourceRead,
}
