use super::*;
use clap::Parser;

#[derive(Parser)]
struct Harness {
    #[command(flatten)]
    args: EditBenchArgs,
}

fn parse(argv: &[&str]) -> EditBenchArgs {
    let mut full = vec!["edit-bench"];
    full.extend_from_slice(argv);
    Harness::try_parse_from(full).unwrap().args
}

#[test]
fn defaults_run_twenty_seeds_in_every_grammar() {
    let options = options(&parse(&[])).unwrap();
    assert_eq!(options.seeds, (0..20).collect::<Vec<_>>());
    assert_eq!(options.grammars, Grammar::ALL.to_vec());
    assert!(options.corpus.is_none());
}

#[test]
fn flags_select_seeds_and_deduplicated_grammars() {
    let options = options(&parse(&[
        "--seeds",
        "3",
        "--first-seed",
        "7",
        "--grammar",
        "patch",
        "--grammar",
        "hashline",
        "--grammar",
        "patch",
    ]))
    .unwrap();
    assert_eq!(options.seeds, vec![7, 8, 9]);
    assert_eq!(options.grammars, vec![Grammar::Patch, Grammar::Hashline]);
}

#[test]
fn bad_grammar_and_seed_counts_are_rejected() {
    let error = options(&parse(&["--grammar", "vim"]))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("unknown grammar") && error.contains("hashline"),
        "{error}"
    );
    assert!(options(&parse(&["--seeds", "0"])).is_err());
    assert!(options(&parse(&["--seeds", "10001"])).is_err());
    assert!(options(&parse(&["--first-seed", "18446744073709551615"])).is_err());
}

#[test]
fn a_run_writes_the_json_report_and_exits_zero_without_wrong_results() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("report.json");
    let args = parse(&[
        "--seeds",
        "1",
        "--grammar",
        "hashline",
        "--report",
        path.to_str().unwrap(),
    ]);
    assert_eq!(run(&args).unwrap(), 0);
    let json: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(json["seeds"], serde_json::json!([0]));
    assert!(
        json["results"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty())
    );
}

#[test]
fn any_wrong_result_makes_the_exit_code_nonzero() {
    use crate::tools::edit::grammar_bench::{CaseResult, Outcome};
    let result = |outcome| CaseResult {
        case: "c".into(),
        scenario: "s".into(),
        grammar: Grammar::Patch,
        outcome,
        call_bytes: 1,
        call_tokens: 1,
        message: None,
    };
    let mut report = Report::default();
    report.results.push(result(Outcome::Refused));
    assert_eq!(exit_code(&report), 0);
    report.results.push(result(Outcome::Wrong));
    assert_eq!(exit_code(&report), 1);
}
