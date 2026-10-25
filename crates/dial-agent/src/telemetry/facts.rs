//! Privacy-preserving projection of controller facts. Never retain payloads, paths or artifact IDs.
use crate::runtime::execution::{Execution, ProcessOutcome};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionFacts {
    pub observation_kind: Option<String>,
    pub observation_satisfied: Option<bool>,
    pub child_outcome: Option<String>,
    pub capture: Option<String>,
    pub retention: Option<String>,
    pub presentation: Option<String>,
    pub original_bytes: Option<u64>,
    pub retained_bytes: Option<u64>,
    pub omitted_bytes: Option<u64>,
    pub presented_bytes: Option<u64>,
    pub connection: Option<String>,
    pub notifications: Option<String>,
    pub catalog: Option<String>,
    pub generation: Option<u64>,
    pub health_transitions: Option<u64>,
    pub state_changes: u64,
    pub dispatch: Option<String>,
    pub settlement: Option<String>,
    pub failure: Option<String>,
}

fn label(value: &impl Serialize) -> String {
    serde_json::to_value(value)
        .expect("runtime enum serialization")
        .as_str()
        .expect("runtime enum label")
        .into()
}

fn child_outcome(p: &ProcessOutcome) -> &'static str {
    if !p.started {
        "not_started"
    } else if p.cancelled {
        "cancelled"
    } else if p.timed_out {
        "timed_out"
    } else if p.signal.is_some() {
        "signalled"
    } else {
        match p.exit_code {
            Some(0) => "succeeded",
            Some(_) => "failed",
            None => "running_or_unknown",
        }
    }
}

impl From<&Execution> for ExecutionFacts {
    fn from(e: &Execution) -> Self {
        let mut facts = Self::default();
        if let Some(o) = &e.observation {
            facts.observation_kind = Some(label(&o.kind));
            facts.observation_satisfied = Some(o.satisfied);
        }
        facts.child_outcome = e.process.as_ref().map(|p| child_outcome(p).into());
        if let Some(o) = &e.output {
            facts.capture = Some(label(&o.capture));
            facts.retention = Some(label(&o.retention));
            facts.presentation = Some(label(&o.presentation));
            facts.original_bytes = o.original_bytes;
            facts.retained_bytes = o.retained_bytes;
            facts.omitted_bytes = o.omitted_bytes;
            facts.presented_bytes = o.presented_bytes;
        }
        if let Some(i) = &e.integration {
            facts.connection = Some(label(&i.health.connection));
            facts.notifications = Some(label(&i.health.notifications));
            facts.catalog = Some(label(&i.health.catalog));
            facts.generation = Some(i.health.generation);
            facts.health_transitions = Some(i.health.transitions);
            facts.state_changes = i.state_changes;
            facts.dispatch = Some(label(&i.dispatch));
            facts.settlement = Some(label(&i.settlement));
            facts.failure = i.failure.as_ref().map(label);
        }
        facts
    }
}

impl ExecutionFacts {
    pub(super) fn labels(&self) -> Vec<(&'static str, String)> {
        let mut labels = Vec::new();
        for (key, value) in [
            ("dial.observation.kind", &self.observation_kind),
            ("dial.process.outcome", &self.child_outcome),
            ("dial.output.capture", &self.capture),
            ("dial.output.retention", &self.retention),
            ("dial.output.presentation", &self.presentation),
            ("dial.integration.connection", &self.connection),
            ("dial.integration.notifications", &self.notifications),
            ("dial.integration.catalog", &self.catalog),
            ("dial.integration.dispatch", &self.dispatch),
            ("dial.integration.settlement", &self.settlement),
            ("dial.integration.failure", &self.failure),
        ] {
            if let Some(value) = value {
                labels.push((key, value.clone()));
            }
        }
        if let Some(satisfied) = self.observation_satisfied {
            labels.push(("dial.observation.satisfied", satisfied.to_string()));
        }
        labels
    }
}
