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
