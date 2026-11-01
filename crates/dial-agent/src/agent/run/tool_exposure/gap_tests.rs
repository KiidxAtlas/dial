use super::*;
use crate::agent::run::tool_exposure::default_offer_tests::with_agent;
use crate::tools::integrations::ext::{Tool, ToolContext};
use crate::tools::surface::catalog::Readiness;
use std::sync::atomic::{AtomicU8, Ordering};

struct Capability {
    name: String,
    description: String,
    readiness: Arc<AtomicU8>,
}
impl Tool for Capability {
    fn name(&self) -> &str {
        &self.name
    }
    fn schema(&self) -> Value {
        json!({"type":"function","function":{"name":self.name,"description":self.description,
            "parameters":{"type":"object","properties":{}}}})
    }
    fn readiness(&self, _: &Config) -> Readiness {
        match self.readiness.load(Ordering::Relaxed) {
            1 => Readiness::Ready,
            2 => Readiness::MissingPrerequisite {
                reason: "not installed".into(),
            },
            3 => Readiness::Unsupported {
                reason: "unsupported host".into(),
            },
            4 => Readiness::ApprovalRequired {
                reason: "startup requires approval".into(),
            },
            _ => Readiness::Unknown,
        }
    }
    fn needs_approval(&self, _: &Value) -> bool {
        true
    }
    fn call(&self, _: &ToolContext, _: &Value) -> ToolOutput {
        panic!("gap routing must never execute a capability")
    }
}

fn register(agent: &mut Agent<'_>, name: &str, description: &str) {
    agent.ext.register(Arc::new(Capability {
        name: name.into(),
        description: description.into(),
        readiness: Arc::new(AtomicU8::new(0)),
    }));
}

/// Each gap maps to one fixed query; a registered integration whose description matches it is
/// offered for the next round, and the same gap activates nothing again in the same turn.
#[test]
fn observed_gap_offers_a_matching_eligible_tool_once_per_turn() {
    with_agent("", |agent| {
        register(
            agent,
            "dap_gapprobe",
            "Debugger runtime diagnostics for a launched program",
        );
        let mut spent = BTreeSet::new();
        let first = agent
            .activate_gap_capabilities(CapabilityGap::DiagnosticFailure, &mut spent)
            .unwrap();
        assert!(first.contains(&"dap_gapprobe".to_owned()), "{first:?}");
        assert!(agent.offered().contains(&"dap_gapprobe".to_owned()));
        let repeat = agent
            .activate_gap_capabilities(CapabilityGap::DiagnosticFailure, &mut spent)
            .unwrap();
        assert!(repeat.is_empty(), "{repeat:?}");
    });
}

/// Explicitly unloaded tools, pinned harness lists and hermetic runs keep gap routing silent.
#[test]
fn gap_routing_honours_unloads_and_pinned_selections() {
    with_agent("", |agent| {
        register(
            agent,
            "dap_unloaded",
            "Debugger runtime diagnostics for a launched program",
        );
        agent.tool_routing.unload(&["dap_unloaded".to_owned()]);
        let mut spent = BTreeSet::new();
        let out = agent
            .activate_gap_capabilities(CapabilityGap::DiagnosticFailure, &mut spent)
            .unwrap();
        assert!(!out.contains(&"dap_unloaded".to_owned()), "{out:?}");
        assert!(!agent.offered().contains(&"dap_unloaded".to_owned()));
    });
    with_agent("[harness]\ntools=['read','bash']", |agent| {
        register(
            agent,
            "dap_pinned",
            "Debugger runtime diagnostics for a launched program",
        );
        let mut spent = BTreeSet::new();
        let out = agent
            .activate_gap_capabilities(CapabilityGap::DiagnosticFailure, &mut spent)
            .unwrap();
        assert!(out.is_empty(), "{out:?}");
        assert!(!agent.offered().contains(&"dap_pinned".to_owned()));
    });
}

/// The candidate builder is the same eligibility boundary `tool_search` uses: a registered
/// integration appears with its description and indexed text, and nothing is activated by listing it.
#[test]
fn eligible_candidates_list_registered_capabilities_without_activating_them() {
    with_agent("", |agent| {
        register(agent, "dap_listed", "Debugger runtime diagnostics");
        let before = agent.offered();
        let candidates = agent.eligible_candidates();
        let entry = candidates
            .iter()
            .find(|(name, _, _)| name == "dap_listed")
            .expect("eligible capability is listed");
        assert_eq!(entry.1, "Debugger runtime diagnostics");
        assert!(entry.2.contains("diagnostics"), "{}", entry.2);
        assert_eq!(agent.offered(), before);
    });
}

#[test]
fn readiness_is_fresh_and_never_hides_explicit_discovery_or_grants_approval() {
    with_agent("", |agent| {
        let state = Arc::new(AtomicU8::new(2));
        agent.ext.register(Arc::new(Capability {
            name: "dap_changing".into(),
            description: "Debugger runtime diagnostics".into(),
            readiness: state.clone(),
        }));
        for (value, automatic) in [(2, false), (1, true), (3, false), (0, true), (4, true)] {
            state.store(value, Ordering::Relaxed);
            assert_eq!(
                agent
                    .automatic_candidates()
                    .iter()
                    .any(|entry| entry.0 == "dap_changing"),
                automatic
            );
            assert!(
                agent
                    .eligible_candidates()
                    .iter()
                    .any(|entry| entry.0 == "dap_changing")
            );
            assert!(
                agent
                    .ext
                    .get("dap_changing")
                    .unwrap()
                    .needs_approval(&json!({}))
            );
        }
        state.store(2, Ordering::Relaxed);
        agent.activate_tools(&["dap_changing".into()]).unwrap();
        assert!(agent.offered().contains(&"dap_changing".into()));
    });
}

#[test]
fn gap_prefers_ready_equivalents_and_unknown_remains_a_candidate() {
    with_agent("", |agent| {
        for (name, state) in [
            ("dap_a_missing", 2),
            ("dap_b_unknown", 0),
            ("dap_c_unknown", 0),
            ("dap_d_unknown", 0),
            ("dap_z_ready", 1),
        ] {
            agent.ext.register(Arc::new(Capability {
                name: name.into(),
                description: "Debugger runtime diagnostics".into(),
                readiness: Arc::new(AtomicU8::new(state)),
            }));
        }
        let activated = agent
            .activate_gap_capabilities(CapabilityGap::DiagnosticFailure, &mut BTreeSet::new())
            .unwrap();
        assert_eq!(activated.first().map(String::as_str), Some("dap_z_ready"));
        assert_eq!(activated.len(), 3);
        assert!(activated.contains(&"dap_b_unknown".into()), "{activated:?}");
        assert!(!activated.contains(&"dap_a_missing".into()));
    });
}
