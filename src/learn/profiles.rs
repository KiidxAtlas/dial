//! Task-specific selection between the current accepted version and its known-good parent.
//! Routes are scoped to the active version; admission or rollback invalidates old routes.
use super::{
    task_policy::Kind,
    workflow::{CaseMeasurement, Metrics},
};
use crate::{config::types::Version, storage::store::Store};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Profile {
    pub samples: usize,
    pub baseline: Metrics,
    pub candidate: Metrics,
    pub baseline_passed: usize,
    pub candidate_passed: usize,
    pub parent_wins: usize,
    pub candidate_wins: usize,
    pub newly_failed: usize,
    pub prefer_parent: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report {
    pub active: i64,
    pub parent: i64,
    pub categories: BTreeMap<Kind, Profile>,
}
fn cheaper(a: &Metrics, b: &Metrics) -> bool {
    a.tool_errors <= b.tool_errors
        && a.protocol_errors <= b.protocol_errors
        && a.repeated_calls <= b.repeated_calls
        && a.tool_calls < b.tool_calls
        && a.generations <= b.generations
}
pub fn measure(
    store: &Store,
    baseline: &Version,
    candidate: &Version,
    pairs: &[(Kind, CaseMeasurement, CaseMeasurement)],
) -> Result<()> {
    let mut categories = BTreeMap::<Kind, Profile>::new();
    for (kind, b, c) in pairs {
        let p = categories.entry(*kind).or_default();
        p.samples += 1;
        p.baseline.add(&b.metrics);
        p.candidate.add(&c.metrics);
        p.baseline_passed += usize::from(b.passed);
        p.candidate_passed += usize::from(c.passed);
        p.newly_failed += usize::from(b.passed && !c.passed);
        if b.passed && c.passed {
            p.parent_wins += usize::from(cheaper(&b.metrics, &c.metrics));
            p.candidate_wins += usize::from(cheaper(&c.metrics, &b.metrics));
        }
    }
    for p in categories.values_mut() {
        // Small or mixed-success samples keep the global winner. No token/time-only routing.
        p.prefer_parent = p.samples >= 8
            && p.baseline_passed == p.samples
            && p.candidate_passed == p.samples
            && p.parent_wins >= 6
            && super::gate::sign_test_p(p.parent_wins, p.candidate_wins) <= 0.01
            && p.baseline.tool_errors <= p.candidate.tool_errors
            && p.baseline.tool_calls < p.candidate.tool_calls;
    }
    let report = Report {
        active: candidate.id,
        parent: baseline.id,
        categories,
    };
    store.set_meta(
        &format!("task_profiles:{}", candidate.id),
        &serde_json::to_string(&report)?,
    )
}
pub fn select(store: &Store, active: Version, prompt: &str) -> Result<Version> {
    let Some(raw) = store.get_meta(&format!("task_profiles:{}", active.id))? else {
        return Ok(active);
    };
    let Ok(report) = serde_json::from_str::<Report>(&raw) else {
        return Ok(active);
    };
    if report.active != active.id
        || active.status != "active"
        || active.parent != Some(report.parent)
    {
        return Ok(active);
    }
    if !report
        .categories
        .get(&Kind::classify(prompt))
        .is_some_and(|p| p.prefer_parent)
    {
        return Ok(active);
    }
    let parent = store.version(report.parent)?;
    if parent.status != "known_good" {
        return Ok(active);
    }
    super::improve::verify_adapter(store, &parent)?;
    Ok(parent)
}

/// Compact operator-facing audit of the currently active routing report.
pub fn summary(store: &Store) -> Result<String> {
    let active = store.active()?;
    let Some(raw) = store.get_meta(&format!("task_profiles:{}", active.id))? else {
        return Ok(
            "Task profiles: collecting paired evaluation evidence; using the accepted default."
                .into(),
        );
    };
    let report: Report = serde_json::from_str(&raw)?;
    let mut lines = vec![
        "Task profiles (correctness-gated; speed alone cannot activate an adapter):".to_owned(),
    ];
    for (kind, p) in report.categories {
        lines.push(format!("{}: v{} · {} paired cases · actions {} → {} · errors {} → {} · inference tokens {} → {} · elapsed {} → {} ms",
            kind.label(),if p.prefer_parent {report.parent}else{active.id},p.samples,
            p.baseline.tool_calls,p.candidate.tool_calls,p.baseline.tool_errors,p.candidate.tool_errors,
            p.baseline.completion_tokens,p.candidate.completion_tokens,p.baseline.elapsed_ms,p.candidate.elapsed_ms));
    }
    Ok(lines.join("\n"))
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{Admission, Harness, SuiteScore};
    #[test]
    fn profiles_require_acceptance_and_expire_on_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), "model", "coding", &Harness::default()).unwrap();
        let base = store.active().unwrap();
        let candidate = store
            .candidate(base.id, "harness", &base.harness, None)
            .unwrap();
        let pairs = (0..8)
            .map(|_| {
                (
                    Kind::Debugging,
                    CaseMeasurement {
                        passed: true,
                        metrics: Metrics {
                            tool_calls: 1,
                            ..Default::default()
                        },
                    },
                    CaseMeasurement {
                        passed: true,
                        metrics: Metrics {
                            tool_calls: 3,
                            ..Default::default()
                        },
                    },
                )
            })
            .collect::<Vec<_>>();
        measure(&store, &base, &candidate, &pairs).unwrap();
        assert_eq!(select(&store, base.clone(), "fix bug").unwrap().id, base.id);
        store
            .admit(&Admission {
                candidate: candidate.id,
                baseline: base.id,
                passed: true,
                reason: "passed".into(),
                scores: vec![
                    SuiteScore {
                        name: "primary".into(),
                        primary: true,
                        baseline: 50.0,
                        candidate: 100.0,
                        cases: 8,
                        baseline_only: 0,
                        candidate_only: 4,
                        tier: None,
                    },
                    SuiteScore {
                        name: "guard".into(),
                        primary: false,
                        baseline: 100.0,
                        candidate: 100.0,
                        cases: 8,
                        baseline_only: 0,
                        candidate_only: 0,
                        tier: None,
                    },
                ],
            })
            .unwrap();
        assert_eq!(
            select(&store, store.active().unwrap(), "fix bug")
                .unwrap()
                .id,
            base.id
        );
        assert_eq!(
            select(&store, store.active().unwrap(), "explain code")
                .unwrap()
                .id,
            candidate.id
        );
        store.rollback().unwrap();
        assert_eq!(
            select(&store, store.active().unwrap(), "fix bug")
                .unwrap()
                .id,
            base.id
        );
    }
}
