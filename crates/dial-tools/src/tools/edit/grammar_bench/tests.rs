use super::*;
use std::fs;

fn file(name: &str) -> SourceFile {
    embedded_corpus()
        .into_iter()
        .find(|file| file.name == name)
        .unwrap()
}

fn options(seeds: std::ops::Range<u64>) -> Options {
    Options {
        seeds: seeds.collect(),
        ..Options::default()
    }
}

#[test]
fn generation_is_deterministic_per_seed_and_varies_across_seeds() {
    let source = file("pricing.rs");
    let describe = |seed| {
        generate(&source, seed)
            .iter()
            .map(|case| format!("{} {:?} {}", case.id, case.hunks, case.shift))
            .collect::<Vec<_>>()
    };
    assert_eq!(describe(3), describe(3));
    assert!((0..8).any(|seed| describe(seed) != describe(3)));
}

#[test]
fn every_kind_and_the_stale_read_scenario_are_generated_for_every_corpus_file() {
    for source in embedded_corpus() {
        for seed in 0..4 {
            let cases = generate(&source, seed);
            for kind in Kind::ALL {
                assert!(
                    cases
                        .iter()
                        .any(|case| case.kind == kind && case.shift == 0),
                    "{} seed {seed} lacks {kind:?}",
                    source.name
                );
            }
            let stale: Vec<_> = cases.iter().filter(|case| case.shift > 0).collect();
            assert_eq!(stale.len(), 1, "{} seed {seed}", source.name);
            assert!((1..=3).contains(&stale[0].shift));
        }
    }
}

#[test]
fn every_case_changes_the_file_and_stays_in_bounds() {
    for source in embedded_corpus() {
        for seed in 0..8 {
            for case in generate(&source, seed) {
                assert_ne!(case.expected(), case.before(), "{} is a no-op", case.id);
                for hunk in &case.hunks {
                    assert!(hunk.start + hunk.remove <= case.lines.len(), "{}", case.id);
                }
            }
        }
    }
}

#[test]
fn rename_cases_are_whole_word_renames_of_every_use() {
    let source = file("pricing.rs");
    let cases: Vec<_> = (0..8)
        .flat_map(|seed| generate(&source, seed))
        .filter(|case| case.kind == Kind::RenameIdentifier)
        .collect();
    assert!(
        cases.iter().any(|case| case.hunks.len() >= 2),
        "some rename should touch a definition and its uses"
    );
    for case in cases {
        let (old, new) = case.rename.clone().unwrap();
        assert_eq!(new, format!("{old}_renamed"));
        assert_eq!(
            case.expected().matches(&new).count(),
            case.original().matches(&old).count(),
            "{}",
            case.id
        );
        assert!(!case.expected().contains(&format!("{old}(")));
    }
}

#[test]
fn hashline_applies_every_case_exactly_even_after_the_file_shifted() {
    let report = run(&Options {
        grammars: vec![Grammar::Hashline],
        ..options(0..6)
    })
    .unwrap();
    assert!(!report.results.is_empty());
    for result in &report.results {
        assert_eq!(result.outcome, Outcome::Exact, "{result:?}");
    }
    assert!(
        report
            .results
            .iter()
            .any(|result| result.scenario == "stale_read")
    );
}

#[test]
fn context_grammars_refuse_instead_of_misapplying_after_the_file_shifted() {
    for grammar in [
        Grammar::Replace,
        Grammar::ApplyPatch,
        Grammar::Patch,
        Grammar::Sloppy,
    ] {
        let report = run(&Options {
            grammars: vec![grammar],
            ..options(0..4)
        })
        .unwrap();
        for result in report
            .results
            .iter()
            .filter(|result| result.scenario == "stale_read")
        {
            assert_ne!(result.outcome, Outcome::Wrong, "{result:?}");
        }
    }
}

#[test]
fn ast_edit_runs_only_rename_cases_and_reports_the_rest_unsupported() {
    let report = run(&Options {
        grammars: vec![Grammar::AstEdit],
        ..options(0..3)
    })
    .unwrap();
    let ran: Vec<_> = report
        .results
        .iter()
        .filter(|result| result.outcome != Outcome::Unsupported)
        .collect();
    assert!(!ran.is_empty());
    assert!(
        ran.iter().all(|result| result.outcome == Outcome::Exact),
        "{ran:?}"
    );
    assert!(
        ran.iter()
            .all(|result| result.case.ends_with("rename_identifier")
                || result.case.contains("stale_read+"))
    );
    assert!(
        report
            .results
            .iter()
            .any(|result| result.outcome == Outcome::Unsupported)
    );
}

#[test]
fn no_grammar_produces_a_wrong_result_on_a_small_seed_set() {
    let report = run(&options(0..5)).unwrap();
    let wrong: Vec<_> = report.wrong().collect();
    assert!(wrong.is_empty(), "unsafe results: {wrong:#?}");
    let totals = report.by_grammar();
    assert_eq!(totals.len(), Grammar::ALL.len());
    for tally in totals.values() {
        assert!(tally.exact > 0);
    }
}

#[test]
#[ignore = "full 200-seed sweep; run with --ignored"]
fn no_grammar_produces_a_wrong_result_on_the_full_seed_set() {
    let report = run(&options(0..200)).unwrap();
    println!("{}", report.render());
    assert_eq!(report.wrong().count(), 0, "{}", report.render());
}

#[test]
fn classification_separates_exact_refused_and_wrong() {
    assert_eq!(classify(false, "b", "a", "b"), Outcome::Exact);
    assert_eq!(classify(true, "a", "a", "b"), Outcome::Refused);
    // Applied the wrong change.
    assert_eq!(classify(false, "c", "a", "b"), Outcome::Wrong);
    // Reported success without making the change.
    assert_eq!(classify(false, "a", "a", "b"), Outcome::Wrong);
    // Reported an error yet changed the file.
    assert_eq!(classify(true, "c", "a", "b"), Outcome::Wrong);
    assert_eq!(classify(true, "b", "a", "b"), Outcome::Wrong);
}

#[test]
fn payload_size_is_the_serialized_call_and_tokens_are_a_quarter_of_it() {
    let report = run(&Options {
        grammars: vec![Grammar::Hashline, Grammar::Patch],
        ..options(0..2)
    })
    .unwrap();
    for result in &report.results {
        assert!(result.call_bytes > 0);
        assert_eq!(result.call_tokens, result.call_bytes.div_ceil(4));
    }
}

#[test]
fn report_renders_tables_and_serializes_to_json() {
    let report = run(&Options {
        grammars: vec![Grammar::Hashline, Grammar::Replace],
        ..options(0..1)
    })
    .unwrap();
    let text = report.render();
    for needle in [
        "grammar",
        "hashline",
        "replace",
        "stale_read / replace",
        "tokens/call",
    ] {
        assert!(text.contains(needle), "{needle} missing from\n{text}");
    }
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["seeds"], serde_json::json!([0]));
    assert_eq!(json["results"][0]["grammar"], "hashline");
    assert!(json["results"][0]["outcome"].is_string());
}

#[test]
fn a_directory_corpus_is_loaded_sorted_and_empty_or_oversized_input_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        load_corpus(dir.path())
            .unwrap_err()
            .to_string()
            .contains("no regular files")
    );
    fs::write(dir.path().join("b.py"), "def beta_one():\n    return 1\n").unwrap();
    fs::write(dir.path().join("a.py"), "def alpha_one():\n    return 1").unwrap();
    fs::create_dir(dir.path().join("nested")).unwrap();
    let files = load_corpus(dir.path()).unwrap();
    assert_eq!(
        files
            .iter()
            .map(|file| file.name.as_str())
            .collect::<Vec<_>>(),
        ["a.py", "b.py"]
    );
    assert!(files[0].text.ends_with('\n'));
    let report = run(&Options {
        corpus: Some(dir.path().to_path_buf()),
        ..options(0..1)
    })
    .unwrap();
    assert!(
        report
            .results
            .iter()
            .any(|result| result.case.starts_with("a.py/"))
    );
    fs::write(dir.path().join("big.py"), vec![b'x'; 300 * 1024]).unwrap();
    assert!(
        load_corpus(dir.path())
            .unwrap_err()
            .to_string()
            .contains("exceeds")
    );
}
