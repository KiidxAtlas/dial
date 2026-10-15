//! Finding comparison. Producer comparison pairs findings across two
//! scans in three tiers: identical fingerprint, then rule plus location key, then same category
//! with an overlapping or nearby (within three lines) location. Lineage then reports how each
//! finding of an earlier scan fared in a later completed scan.

use anyhow::{Result, bail};

use super::contracts::{Finding, Scan, ScanStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Fingerprint,
    RuleLocation,
    TaxonomyLocation,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fingerprint => "fingerprint",
            Self::RuleLocation => "rule-location",
            Self::TaxonomyLocation => "taxonomy-location",
        }
    }
}

/// A matched before/after pair of finding indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pair {
    pub before: usize,
    pub after: usize,
    pub method: Method,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comparison {
    pub matched: Vec<Pair>,
    pub before_only: Vec<usize>,
    pub after_only: Vec<usize>,
}

fn primary_path(finding: &Finding) -> Option<String> {
    finding
        .locations
        .first()
        .map(|l| l.path.replace('\\', "/").to_lowercase())
}

fn primary_line(finding: &Finding) -> Option<u64> {
    finding.locations.first().and_then(|l| l.start_line)
}

fn rule_location_key(finding: &Finding) -> Option<String> {
    let path = primary_path(finding)?;
    let line = primary_line(finding)?;
    Some(format!(
        "{}\0{}:{}",
        finding.rule_id.to_lowercase(),
        path,
        line
    ))
}

fn taxonomy_location_match(a: &Finding, b: &Finding) -> bool {
    if a.category.trim().to_lowercase() != b.category.trim().to_lowercase() {
        return false;
    }
    a.locations.iter().any(|la| {
        b.locations.iter().any(|lb| {
            if la.path.replace('\\', "/").to_lowercase()
                != lb.path.replace('\\', "/").to_lowercase()
            {
                return false;
            }
            match (la.start_line, lb.start_line) {
                (Some(x), Some(y)) => x.abs_diff(y) <= 3,
                _ => true,
            }
        })
    })
}

/// Pairs findings greedily in tier order, so each finding is matched at most once.
pub fn compare_findings(before: &[Finding], after: &[Finding]) -> Comparison {
    let mut before_used = vec![false; before.len()];
    let mut after_used = vec![false; after.len()];
    let mut matched = Vec::new();

    let tiers: [Method; 3] = [
        Method::Fingerprint,
        Method::RuleLocation,
        Method::TaxonomyLocation,
    ];
    for method in tiers {
        for (ai, a) in after.iter().enumerate() {
            if after_used[ai] {
                continue;
            }
            let found = before.iter().enumerate().find(|(bi, b)| {
                if before_used[*bi] {
                    return false;
                }
                match method {
                    Method::Fingerprint => a.fingerprint == b.fingerprint,
                    Method::RuleLocation => match (rule_location_key(a), rule_location_key(b)) {
                        (Some(x), Some(y)) => x == y,
                        _ => false,
                    },
                    Method::TaxonomyLocation => taxonomy_location_match(a, b),
                }
            });
            if let Some((bi, _)) = found {
                before_used[bi] = true;
                after_used[ai] = true;
                matched.push(Pair {
                    before: bi,
                    after: ai,
                    method,
                });
            }
        }
    }
    matched.sort_by_key(|p| (p.after, p.before));

    Comparison {
        matched,
        before_only: before_used
            .iter()
            .enumerate()
            .filter(|(_, u)| !**u)
            .map(|(i, _)| i)
            .collect(),
        after_only: after_used
            .iter()
            .enumerate()
            .filter(|(_, u)| !**u)
            .map(|(i, _)| i)
            .collect(),
    }
}

/// Lineage entry: how a finding relates across an earlier and a later scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineageEntry {
    pub fingerprint: String,
    pub title: String,
    pub status: &'static str,
    pub method: Option<Method>,
}

/// Compares a completed after-scan against the before-scan. Entries are sorted by
/// fingerprint. Statuses: `unchanged` (matched), `new` (only in after), `resolved` (only in before).
pub fn lineage(before: &Scan, after: &Scan) -> Result<Vec<LineageEntry>> {
    if after.status != ScanStatus::Completed {
        bail!(
            "Security lineage comparison requires a completed after-scan; {} is {}",
            after.id,
            after.status.as_str()
        );
    }
    let comparison = compare_findings(&before.findings, &after.findings);
    let mut entries = Vec::new();
    for pair in &comparison.matched {
        let finding = &after.findings[pair.after];
        entries.push(LineageEntry {
            fingerprint: finding.fingerprint.clone(),
            title: finding.title.clone(),
            status: "unchanged",
            method: Some(pair.method),
        });
    }
    for index in &comparison.after_only {
        let finding = &after.findings[*index];
        entries.push(LineageEntry {
            fingerprint: finding.fingerprint.clone(),
            title: finding.title.clone(),
            status: "new",
            method: None,
        });
    }
    for index in &comparison.before_only {
        let finding = &before.findings[*index];
        entries.push(LineageEntry {
            fingerprint: finding.fingerprint.clone(),
            title: finding.title.clone(),
            status: "resolved",
            method: None,
        });
    }
    entries.sort_by(|a, b| {
        a.fingerprint
            .cmp(&b.fingerprint)
            .then(a.status.cmp(b.status))
    });
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::contracts::{Confidence, Disposition, Location, Producer, Severity};

    fn finding(id: &str, fp: &str, rule: &str, category: &str, path: &str, line: u64) -> Finding {
        Finding {
            id: id.into(),
            fingerprint: fp.into(),
            rule_id: rule.into(),
            title: format!("title {id}"),
            category: category.into(),
            severity: Severity::High,
            confidence: Confidence::High,
            disposition: Disposition::Open,
            locations: vec![Location {
                path: path.into(),
                start_line: Some(line),
                end_line: None,
                start_column: None,
                end_column: None,
                role: None,
            }],
            anchor: None,
            description: None,
            validation_status: None,
            validation_summary: None,
            validation_evidence: vec![],
        }
    }

    fn scan(id: &str, status: ScanStatus, findings: Vec<Finding>) -> Scan {
        Scan {
            document_type: "x".into(),
            schema_version: "1.0".into(),
            id: id.into(),
            project_key: "k".into(),
            repository_root: "/r".into(),
            target_kind: crate::security::contracts::TargetKind::Repository,
            status,
            created_at: "t".into(),
            completed_at: None,
            producer: Producer {
                kind: "k".into(),
                name: "n".into(),
                vendor: "v".into(),
                version: None,
            },
            plan_id: None,
            revision: None,
            cloud: None,
            findings,
        }
    }

    #[test]
    fn tiers_match_fingerprint_then_rule_location_then_taxonomy() {
        let before = vec![
            finding("a", "fp1", "r1", "cat", "src/x.rs", 10),
            finding("b", "fp2", "r2", "cat", "src/y.rs", 20),
            finding("c", "fp3", "r3", "cat", "src/z.rs", 30),
        ];
        let after = vec![
            finding("A", "fp1", "r1", "cat", "src/x.rs", 10),
            finding("B", "new2", "R2", "cat", "src/y.rs", 20),
            finding("C", "new3", "other", "cat", "src/z.rs", 32),
            finding("D", "new4", "zz", "cat", "src/q.rs", 1),
        ];
        let cmp = compare_findings(&before, &after);
        let methods: Vec<Method> = cmp.matched.iter().map(|p| p.method).collect();
        assert_eq!(
            methods,
            vec![
                Method::Fingerprint,
                Method::RuleLocation,
                Method::TaxonomyLocation
            ]
        );
        assert_eq!(cmp.after_only, vec![3]);
        assert!(cmp.before_only.is_empty());
    }

    #[test]
    fn lineage_rejects_incomplete_after_scan() {
        let before = scan("secscan_aa", ScanStatus::Completed, vec![]);
        let after = scan("secscan_bb", ScanStatus::Running, vec![]);
        let error = lineage(&before, &after).unwrap_err().to_string();
        assert_eq!(
            error,
            "Security lineage comparison requires a completed after-scan; secscan_bb is running"
        );
    }

    #[test]
    fn lineage_reports_new_resolved_unchanged_sorted() {
        let before = scan(
            "secscan_aa",
            ScanStatus::Completed,
            vec![
                finding("a", "fp-a", "r1", "c", "x", 1),
                finding("b", "fp-b", "r2", "c", "y", 1),
            ],
        );
        let after = scan(
            "secscan_bb",
            ScanStatus::Completed,
            vec![
                finding("c", "fp-a", "r1", "c", "x", 1),
                finding("d", "fp-d", "r9", "c", "w", 1),
            ],
        );
        let entries = lineage(&before, &after).unwrap();
        let summary: Vec<(String, &str)> = entries
            .iter()
            .map(|e| (e.fingerprint.clone(), e.status))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("fp-a".to_owned(), "unchanged"),
                ("fp-b".to_owned(), "resolved"),
                ("fp-d".to_owned(), "new"),
            ]
        );
    }
}
