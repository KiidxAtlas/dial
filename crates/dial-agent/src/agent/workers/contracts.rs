//! Durable workflow intent and dependency bindings; saved verdicts never restore authority.
use crate::{
    agent::verify::worker_acceptance::{Acceptance, State as AcceptanceState},
    config::types::VerificationConfig,
    tools::Workspace,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

const MAX_NODES: usize = 32;
const MAX_ATTEMPTS: u8 = 3;
const MAX_REQUEST_BYTES: usize = 32 * 1024;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::agent) struct Contract {
    pub id: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum State {
    Starting,
    Running,
    Accepted,
    NeedsVerification,
    Retryable,
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Dependency {
    output: String,
    binding: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Node {
    id: String,
    request: Value,
    depends_on: BTreeMap<String, Dependency>,
    attempts: u8,
    state: State,
    worker: Option<String>,
    output: Option<String>,
    binding: Option<String>,
    reason: String,
    // Runtime-only authority. Neither a saved state nor a model-authored receipt can populate it.
    #[serde(skip)]
    receipt: Option<Acceptance>,
}

#[derive(Debug, Default)]
pub(in crate::agent) struct Ledger {
    nodes: BTreeMap<String, Node>,
    fault: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    version: u8,
    nodes: BTreeMap<String, Node>,
    fault: Option<String>,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
}

impl Ledger {
    pub(in crate::agent) fn restore(value: &Value) -> Self {
        if value.is_null() {
            return Self::default();
        }
        let saved = serde_json::from_value::<Saved>(value.clone());
        let mut ledger = Self::default();
        let Ok(saved) = saved else {
            ledger
                .fail("workflow journal is malformed; inspect the session before new work".into());
            return ledger;
        };
        if saved.version != 1 || saved.nodes.len() > MAX_NODES {
            ledger.fail("workflow journal version or size is unsupported".into());
            return ledger;
        }
        ledger.nodes = saved.nodes;
        ledger.fault = saved.fault;
        for (id, node) in &mut ledger.nodes {
            if !valid_id(id)
                || id != &node.id
                || node.attempts == 0
                || node.attempts > MAX_ATTEMPTS
                || node.request.to_string().len() > MAX_REQUEST_BYTES
                || node.depends_on.len() > 16
                || node
                    .depends_on
                    .keys()
                    .any(|dep| !valid_id(dep) || dep == id)
            {
                ledger.fault = Some("workflow journal has invalid node records".into());
            }
            match node.state {
                State::Starting | State::Running => {
                    node.state = State::Interrupted;
                    node.reason = "interrupted execution: inspect retained work; effects will not be replayed".into();
                }
                State::Accepted => {
                    node.state = State::NeedsVerification;
                    node.reason = "saved acceptance is historical; verify retained output with current checks".into();
                }
                _ => {}
            }
        }
        if ledger.nodes.values().any(|node| {
            node.depends_on
                .keys()
                .any(|dep| !ledger.nodes.contains_key(dep))
        }) {
            ledger.fail("workflow journal refers to a missing dependency".into());
        }
        ledger
    }

    pub(in crate::agent) fn fail(&mut self, reason: String) {
        self.fault.get_or_insert(reason);
    }

    pub(in crate::agent) fn snapshot(&self) -> Value {
        if self.nodes.is_empty() && self.fault.is_none() {
            return Value::Null;
        }
        json!(Saved {
            version: 1,
            nodes: self.nodes.clone(),
            fault: self.fault.clone()
        })
    }

    pub(in crate::agent) fn blocker(&self) -> Option<String> {
        if let Some(fault) = &self.fault {
            return Some(format!("Cannot complete contracted workflow: {fault}"));
        }
        self.nodes
            .values()
            .find(|node| node.state != State::Accepted)
            .map(|node| {
                format!(
                    "Cannot complete contracted workflow node {:?}: {}",
                    node.id, node.reason
                )
            })
    }

    fn dependencies(&self, ids: &[String]) -> Result<BTreeMap<String, Dependency>, String> {
        if ids.len() > 16 || ids.iter().collect::<BTreeSet<_>>().len() != ids.len() {
            return Err("contract needs at most 16 unique dependencies".into());
        }
        ids.iter()
            .map(|id| {
                let node = self
                    .nodes
                    .get(id)
                    .ok_or_else(|| format!("unknown dependency {id:?}"))?;
                if node.state != State::Accepted {
                    return Err(format!("dependency {id:?} is not currently accepted"));
                }
                let dependency = Dependency {
                    output: node
                        .output
                        .clone()
                        .ok_or("dependency has no retained output")?,
                    binding: node
                        .binding
                        .clone()
                        .ok_or("dependency has no evidence binding")?,
                };
                Ok((id.clone(), dependency))
            })
            .collect()
    }

    pub(in crate::agent) fn begin(
        &mut self,
        contract: &Contract,
        request: Value,
    ) -> Result<(), String> {
        if let Some(fault) = &self.fault {
            return Err(fault.clone());
        }
        if !valid_id(&contract.id) || contract.depends_on.contains(&contract.id) {
            return Err("contract id must be 1..64 ASCII letters, digits, '-', '_' or '.' and cannot depend on itself".into());
        }
        if request.to_string().len() > MAX_REQUEST_BYTES {
            return Err("contract assignment exceeds 32 KiB".into());
        }
        let dependencies = self.dependencies(&contract.depends_on)?;
        let attempts = if let Some(node) = self.nodes.get(&contract.id) {
            if node.request != request || node.depends_on.keys().ne(dependencies.keys()) {
                return Err("contract assignment and dependency names are immutable; do not change the acceptance target".into());
            }
            if !matches!(node.state, State::Retryable | State::NeedsVerification) {
                return Err(
                    "node is accepted, running or interrupted; it cannot be replayed".into(),
                );
            }
            if node.attempts >= MAX_ATTEMPTS {
                return Err("workflow node exhausted its three explicit attempts".into());
            }
            node.attempts + 1
        } else {
            if self.nodes.len() >= MAX_NODES {
                return Err("workflow has reached its 32 node limit".into());
            }
            1
        };
        self.nodes.insert(
            contract.id.clone(),
            Node {
                id: contract.id.clone(),
                request,
                depends_on: dependencies,
                attempts,
                state: State::Starting,
                worker: None,
                output: None,
                binding: None,
                reason: "durable launch intent recorded; worker not yet attached".into(),
                receipt: None,
            },
        );
        self.invalidate_dependencies();
        Ok(())
    }

    pub(in crate::agent) fn attach(&mut self, id: &str, worker: &str) -> Result<(), String> {
        let node = self.nodes.get_mut(id).ok_or("unknown workflow node")?;
        if node.state != State::Starting || worker.is_empty() {
            return Err("worker can only attach once to a fresh durable intent".into());
        }
        node.worker = Some(worker.into());
        node.state = State::Running;
        node.reason = "worker is running; acceptance not established".into();
        Ok(())
    }

    pub(in crate::agent) fn running(&self) -> Vec<(String, String)> {
        self.nodes
            .values()
            .filter(|node| node.state == State::Running)
            .filter_map(|node| Some((node.id.clone(), node.worker.clone()?)))
            .collect()
    }

    pub(in crate::agent) fn settle(
        &mut self,
        id: &str,
        status: &str,
        receipt: Acceptance,
        output: Option<String>,
    ) -> Result<(), String> {
        let node = self.nodes.get_mut(id).ok_or("unknown workflow node")?;
        if !matches!(node.state, State::Starting | State::Running) {
            return Err("workflow node cannot settle twice".into());
        }
        match status {
            "done"
                if receipt.state == AcceptanceState::Accepted
                    && output
                        .as_deref()
                        .is_some_and(|uri| uri.starts_with("artifact://")) =>
            {
                node.binding = receipt.binding();
                node.output = output;
                node.reason = receipt.reason.clone();
                node.state = if node.binding.is_some() {
                    State::Accepted
                } else {
                    State::NeedsVerification
                };
                node.receipt = Some(receipt);
            }
            "never_started" => {
                node.state = State::Retryable;
                node.reason = "worker did not start; an explicit bounded retry is available".into();
            }
            "done" | "failed" if receipt.state == AcceptanceState::Rejected => {
                node.state = State::Retryable;
                node.reason = receipt.reason;
            }
            _ => {
                node.state = State::Interrupted;
                node.reason = "execution did not establish acceptance; inspect retained work and effects before new work".into();
            }
        }
        self.invalidate_dependencies();
        Ok(())
    }

    fn invalidate_dependencies(&mut self) {
        // Repeated propagation handles arbitrary topological order without replaying any work.
        for _ in 0..self.nodes.len() {
            let stale: Vec<_> = self
                .nodes
                .values()
                .filter(|node| {
                    matches!(node.state, State::Accepted | State::NeedsVerification)
                        && node.depends_on.iter().any(|(id, binding)| {
                            self.nodes.get(id).is_none_or(|dep| {
                                dep.state != State::Accepted
                                    || dep.output.as_ref() != Some(&binding.output)
                                    || dep.binding.as_ref() != Some(&binding.binding)
                            })
                        })
                })
                .filter(|node| node.receipt.is_some() || node.state == State::Accepted)
                .map(|node| node.id.clone())
                .collect();
            if stale.is_empty() {
                break;
            }
            for id in stale {
                if let Some(node) = self.nodes.get_mut(&id) {
                    node.state = State::NeedsVerification;
                    node.receipt = None;
                    node.reason = "dependency acceptance or output changed; explicit verification or retry required".into();
                }
            }
        }
    }

    pub(in crate::agent) fn refresh(&mut self, workspace: &Workspace, config: &VerificationConfig) {
        for node in self.nodes.values_mut() {
            if node.state == State::Accepted {
                if let Some(receipt) = &mut node.receipt {
                    receipt.revalidate(workspace, config);
                    if receipt.state != AcceptanceState::Accepted {
                        node.state = State::NeedsVerification;
                        node.reason = receipt.reason.clone();
                    }
                } else {
                    node.state = State::NeedsVerification;
                    node.reason = "current controller receipt is unavailable".into();
                }
                if node.output.as_deref().is_none_or(|uri| {
                    workspace.artifacts().is_none_or(|store| {
                        store.info(uri).map_or(true, |info| {
                            info["state"] != "retained" && info["state"] != "present_legacy"
                        })
                    })
                }) {
                    node.state = State::NeedsVerification;
                    node.reason = "retained output is unavailable; inspection required".into();
                }
            }
        }
        self.invalidate_dependencies();
    }

    pub(in crate::agent) fn node(&self, id: &str) -> Option<Value> {
        self.nodes.get(id).map(|node| json!(node))
    }

    pub(in crate::agent) fn checks(&self, id: &str) -> Result<Vec<String>, String> {
        if let Some(fault) = &self.fault {
            return Err(fault.clone());
        }
        let node = self.nodes.get(id).ok_or("unknown workflow node")?;
        if !matches!(node.state, State::Accepted | State::NeedsVerification)
            || node.output.is_none()
            || node.binding.is_none()
        {
            return Err("only previously accepted retained output can be reverified; never replay interrupted work".into());
        }
        let ids: Vec<_> = node.depends_on.keys().cloned().collect();
        if self.dependencies(&ids)? != node.depends_on {
            return Err(
                "dependency binding changed; retained result needs an explicit new attempt".into(),
            );
        }
        serde_json::from_value(node.request["acceptance_checks"].clone())
            .map_err(|_| "saved acceptance check IDs are malformed".into())
    }

    pub(in crate::agent) fn rechecked(
        &mut self,
        id: &str,
        receipt: Acceptance,
    ) -> Result<(), String> {
        self.checks(id)?;
        let node = self.nodes.get_mut(id).ok_or("unknown workflow node")?;
        if receipt.state != AcceptanceState::Accepted || receipt.binding() != node.binding {
            node.state = State::NeedsVerification;
            node.reason =
                "fresh checks did not match retained output's original input and check binding"
                    .into();
            node.receipt = None;
        } else {
            node.state = State::Accepted;
            node.reason = receipt.reason.clone();
            node.receipt = Some(receipt);
        }
        self.invalidate_dependencies();
        Ok(())
    }
}

#[cfg(test)]
mod tests;
