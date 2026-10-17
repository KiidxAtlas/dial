use super::*;

fn detail(text: &str) -> ConventionalDetail {
    ConventionalDetail {
        text: text.to_owned(),
        changelog_category: None,
        user_visible: None,
    }
}

#[test]
fn summary_rules_reject_present_tense_and_trailing_period() {
    let checked = validate_summary_rules("Adds parser.");
    assert!(
        checked
            .errors
            .iter()
            .any(|error| error.contains("past-tense"))
    );
    assert!(checked.errors.iter().any(|error| error.contains("period")));
}

#[test]
fn summary_rules_warn_on_filler_and_meta() {
    let checked = validate_summary_rules("Added comprehensive this change support");
    assert!(checked.warnings.iter().any(|w| w.contains("comprehensive")));
    assert!(checked.warnings.iter().any(|w| w.contains("this change")));
}

#[test]
fn scope_rejects_uppercase_and_three_segments() {
    assert!(!validate_scope(Some("Core")).is_empty());
    assert!(!validate_scope(Some("a/b/c")).is_empty());
    assert!(validate_scope(Some("api/http-v2")).is_empty());
}

#[test]
fn details_require_period_and_length() {
    let errors = validate_details(&[detail("no period"), detail(&"x".repeat(121))]);
    assert_eq!(errors.len(), 3);
}

#[test]
fn cap_details_keeps_highest_priority_in_order() {
    let details: Vec<ConventionalDetail> = (0..8)
        .map(|i| {
            detail(&if i == 7 {
                "Fixed crash.".to_owned()
            } else {
                format!("Plain item {i}.")
            })
        })
        .collect();
    let (kept, warnings) = cap_details(&details);
    assert_eq!(kept.len(), MAX_DETAIL_ITEMS);
    assert!(kept.iter().any(|d| d.text == "Fixed crash."));
    assert_eq!(warnings.len(), 1);
}

#[test]
fn type_consistency_requires_docs_files_for_docs() {
    let checked =
        validate_type_consistency("docs", &["src/lib.rs".to_owned()], "", "Documented x", &[]);
    assert_eq!(checked.errors.len(), 1);
    let ok = validate_type_consistency("docs", &["README.md".to_owned()], "", "Documented x", &[]);
    assert!(ok.errors.is_empty());
}

#[test]
fn type_consistency_warns_perf_without_evidence() {
    let checked =
        validate_type_consistency("perf", &["src/a.rs".to_owned()], "", "Sped up parsing", &[]);
    assert_eq!(checked.warnings.len(), 1);
}
