use super::*;
use crate::config::types::{Harness, SuiteConfig};
use std::fs;
use tempfile::TempDir;

fn versions() -> (Version, Version) {
    let baseline = Version {
        id: 1,
        parent: None,
        kind: "bootstrap".into(),
        harness: Harness {
            system_prompt: "original".into(),
            memory: vec![],
            tools: vec![],
            subagents: vec![],
            max_steps: 1,
            temperature: 0.0,
            max_tokens: 8,
            edit_mode: None,
        },
        adapter: None,
        status: "active".into(),
    };
    let mut candidate = baseline.clone();
    candidate.id = 2;
    candidate.parent = Some(1);
    candidate.status = "candidate".into();
    candidate.harness.system_prompt = "improved".into();
    (baseline, candidate)
}

fn rules() -> Rules {
    Rules {
        max_regression: 0.0,
        min_improvement: 1.0,
        alpha: 1.0,
        min_net_wins: 0,
        stop_when_futile: false,
    }
}

fn case(id: &str, prompt: &str, expected: &str) -> Case {
    Case {
        id: id.into(),
        prompt: prompt.into(),
        files: Files::new(),
        check: Check::AnswerExact {
            expected: expected.into(),
        },
    }
}

fn suite(name: &str, primary: bool, n: usize) -> Suite {
    Suite {
        name: name.into(),
        primary,
        cases: (0..n)
            .map(|i| case(&format!("{name}{i}"), &format!("{name} prompt {i}"), "yes"))
            .collect(),
    }
}

/// Candidate solves exactly the cases of `suite` whose index is below `cand`; baseline those below `base`.
fn runner<'a>(
    base: usize,
    cand: usize,
    suite_name: &'a str,
) -> impl Fn(&Version, &Case) -> Result<bool> + 'a {
    move |v, c| {
        let Some(index) =
            c.id.strip_prefix(suite_name)
                .and_then(|i| i.parse::<usize>().ok())
        else {
            return Ok(true);
        };
        Ok(index < if v.id == 2 { cand } else { base })
    }
}

#[test]
fn sign_test_matches_exact_binomial_tails() {
    assert_eq!(sign_test_p(0, 0), 1.0);
    assert_eq!(sign_test_p(0, 7), 1.0);
    assert_eq!(sign_test_p(1, 0), 0.5);
    assert_eq!(sign_test_p(5, 0), 1.0 / 32.0);
    assert_eq!(sign_test_p(3, 1), 5.0 / 16.0);
    assert_eq!(sign_test_p(8, 2), 56.0 / 1024.0);
    assert_eq!(sign_test_p(4, 4), 163.0 / 256.0);
}

#[test]
fn sign_test_is_monotonic() {
    for losses in [0, 1, 5, 40, 150] {
        let mut last = f64::INFINITY;
        for wins in 0..200 {
            let p = sign_test_p(wins, losses);
            assert!(
                (0.0..=1.0).contains(&p),
                "p out of range for {wins} vs {losses}"
            );
            assert!(
                p <= last,
                "p must not grow with more wins ({wins} vs {losses})"
            );
            last = p;
        }
    }
    for wins in [1, 5, 40] {
        let mut last = 0.0;
        for losses in 0..200 {
            let p = sign_test_p(wins, losses);
            assert!(
                p >= last,
                "p must not shrink with more losses ({wins} vs {losses})"
            );
            last = p;
        }
    }
}

#[test]
fn sign_test_is_stable_for_large_samples() {
    // Continuity across the exact / log-space boundary.
    assert!((sign_test_p(60, 41) - sign_test_p(60, 40)).abs() < 0.1);
    let (exact, logspace) = (sign_test_p(55, 45), sign_test_p(56, 45));
    assert!((0.1..0.2).contains(&exact) && logspace < exact && exact - logspace < 0.05);
    let even = sign_test_p(2500, 2500);
    assert!((0.5..0.52).contains(&even), "{even}");
    let lead = sign_test_p(2600, 2400);
    assert!(lead > 0.0 && lead < 0.01, "{lead}");
    assert_eq!(sign_test_p(5000, 0), 0.0);
    assert!((sign_test_p(1, 4999) - 1.0).abs() < 1e-12);
}

#[test]
fn primary_gain_does_not_excuse_guard_regression() {
    let (baseline, candidate) = versions();
    let suites = [suite("primary", true, 2), suite("guard", false, 2)];
    // Candidate solves p0,p1 (baseline none); baseline solves guard g0, candidate not.
    let report = evaluate(
        &rules(),
        &suites,
        &baseline,
        &candidate,
        &|| false,
        |v, c| {
            Ok(matches!(
                (v.id, c.id.as_str()),
                (2, "primary0" | "primary1") | (1, "guard0" | "guard1")
            ))
        },
    )
    .unwrap();
    assert!(!report.passed);
    assert_eq!(report.scores[0].candidate, 100.0);
    assert_eq!(report.scores[0].candidate_only, 2);
    assert_eq!(report.scores[1].baseline, 100.0);
    assert_eq!(report.scores[1].baseline_only, 2);
    assert!(report.reason.contains("regression"));
    assert!(!serde_json::to_string(&report).unwrap().contains("prompt"));
}

#[test]
fn tolerances_are_percentage_points_with_inclusive_boundaries() {
    let (baseline, candidate) = versions();
    let suites = [suite("primary", true, 1), suite("guard", false, 100)];
    // Primary: candidate solves its only case, baseline fails it. Guard: candidate loses guard0.
    let run = |v: &Version, c: &Case| -> Result<bool> {
        Ok(if c.id == "primary0" {
            v.id == 2
        } else {
            !(c.id == "guard0" && v.id == 2)
        })
    };
    let at = |max_regression, min_improvement| {
        let r = Rules {
            max_regression,
            min_improvement,
            ..rules()
        };
        evaluate(&r, &suites, &baseline, &candidate, &|| false, run)
            .unwrap()
            .passed
    };
    assert!(at(1.0, 100.0));
    assert!(!at(0.99, 100.0));
    assert!(!at(1.0, 100.01));
    let error = evaluate(
        &Rules {
            min_improvement: 0.0,
            ..rules()
        },
        &suites,
        &baseline,
        &candidate,
        &|| false,
        run,
    )
    .unwrap_err();
    assert!(error.to_string().contains("min_improvement"));
}

#[test]
fn failures_are_classified_by_cause() {
    let class = |text: &str| classify_failure(&anyhow::anyhow!("{text}"));
    assert_eq!(
        class("model server returned HTTP 429: rate limited upstream"),
        FailureKind::Infra("transport_429")
    );
    assert_eq!(
        class("model server returned HTTP 503: service unavailable"),
        FailureKind::Infra("transport_5xx")
    );
    assert_eq!(
        class("model server returned HTTP 400: Failed to load model"),
        FailureKind::Infra("model_load")
    );
    assert_eq!(
        classify_failure(&anyhow::Error::new(ThinkingLoop("loop".into()))),
        FailureKind::Model
    );
    assert_eq!(class("SECRET prompt and output"), FailureKind::Fatal);
}

#[test]
fn transient_failure_recovers_on_retry_without_exclusion() {
    let (baseline, candidate) = versions();
    let suites = [suite("primary", true, 10)];
    let attempts = std::cell::Cell::new(0);
    let report = evaluate_with(
        &rules(),
        &suites,
        &baseline,
        &candidate,
        |v, c| {
            if v.id == 2 && c.id == "primary2" && attempts.get() == 0 {
                attempts.set(1);
                bail!("model server returned HTTP 503: service unavailable");
            }
            Ok(true)
        },
        &[Duration::ZERO; 2],
        &|| false,
    )
    .unwrap();
    assert_eq!(attempts.get(), 1);
    assert_eq!(report.scores[0].cases, 10);
    assert!(!report.reason.contains("excluded"), "{}", report.reason);
}

#[test]
fn persistent_candidate_infrastructure_failure_excludes_the_pair() {
    let (baseline, candidate) = versions();
    let suites = [suite("primary", true, 10)];
    let attempts = std::cell::Cell::new(0);
    let report = evaluate_with(
        &rules(),
        &suites,
        &baseline,
        &candidate,
        |v, c| {
            if v.id == 2 && c.id == "primary3" {
                attempts.set(attempts.get() + 1);
                bail!("model server returned HTTP 429: rate limited upstream");
            }
            Ok(true)
        },
        &[Duration::ZERO; 2],
        &|| false,
    )
    .unwrap();
    assert_eq!(attempts.get(), 3);
    assert_eq!(report.scores[0].cases, 9);
    assert!(
        report.reason.contains("1 case pairs excluded"),
        "{}",
        report.reason
    );
    assert!(
        report.reason.contains("transport_429 x1"),
        "{}",
        report.reason
    );
}

#[test]
fn baseline_infrastructure_failure_skips_the_candidate_for_that_pair() {
    let (baseline, candidate) = versions();
    let suites = [suite("primary", true, 5)];
    let candidate_calls = std::cell::Cell::new(0);
    let report = evaluate_with(
        &rules(),
        &suites,
        &baseline,
        &candidate,
        |v, c| {
            if v.id == 1 && c.id == "primary0" {
                bail!("model server returned HTTP 503: service unavailable");
            }
            if v.id == 2 && c.id == "primary0" {
                candidate_calls.set(candidate_calls.get() + 1);
            }
            Ok(true)
        },
        &[Duration::ZERO; 2],
        &|| false,
    )
    .unwrap();
    assert_eq!(candidate_calls.get(), 0);
    assert_eq!(report.scores[0].cases, 4);
}

#[test]
fn excessive_infrastructure_failures_abort_the_evaluation() {
    let (baseline, candidate) = versions();
    let suites = [suite("primary", true, 10)];
    let error = evaluate_with(
        &rules(),
        &suites,
        &baseline,
        &candidate,
        |v, c| {
            if v.id == 2 && ["primary0", "primary1", "primary2"].contains(&c.id.as_str()) {
                bail!("model server returned HTTP 503: service unavailable");
            }
            Ok(true)
        },
        &[],
        &|| false,
    )
    .expect_err("3 of 10 excluded cases must abort");
    assert!(error.to_string().contains("did not complete"), "{error}");
}

#[test]
fn model_failures_still_count_against_the_candidate() {
    let (baseline, candidate) = versions();
    let suites = [suite("primary", true, 10)];
    let report = evaluate_with(
        &rules(),
        &suites,
        &baseline,
        &candidate,
        |v, c| {
            if v.id == 2 && c.id == "primary0" {
                return Err(anyhow::Error::new(ThinkingLoop("loop".into())));
            }
            Ok(true)
        },
        &[],
        &|| false,
    )
    .unwrap();
    assert_eq!(report.scores[0].cases, 10);
    assert_eq!(report.scores[0].baseline, 100.0);
    assert_eq!(report.scores[0].candidate, 90.0);
    assert!(!report.reason.contains("excluded"), "{}", report.reason);
}

#[test]
fn failures_are_sealed() {
    let (baseline, candidate) = versions();
    let suites = [suite("primary", true, 1), suite("guard", false, 1)];
    let error = evaluate(
        &rules(),
        &suites,
        &baseline,
        &candidate,
        &|| false,
        |_, _| bail!("SECRET prompt and output"),
    )
    .unwrap_err();
    assert!(!format!("{error:#}").contains("SECRET"));
}

#[test]
fn invalid_input_is_rejected_before_any_inference() {
    let (baseline, candidate) = versions();
    let never =
        |_: &Version, _: &Case| -> Result<bool> { panic!("must validate before inference") };
    let good = || [suite("primary", true, 1), suite("guard", false, 1)];

    let mut dup_ids = good();
    dup_ids[0].cases.push(dup_ids[0].cases[0].clone());
    let mut empty = good();
    empty[1].cases.clear();
    let mut dup_names = good();
    dup_names[1].name = "primary".into();
    let mut bad_file = good();
    bad_file[1].cases[0]
        .files
        .insert("../escape".into(), "x".into());
    let mut many_files = good();
    many_files[1].cases[0].files = (0..65).map(|i| (format!("f{i}"), String::new())).collect();
    let mut big_file = good();
    big_file[1].cases[0]
        .files
        .insert("big".into(), "x".repeat(256 * 1024 + 1));
    let mut two_primary = good();
    two_primary[1].primary = true;
    let mut no_primary = good();
    no_primary[0].primary = false;
    let mut blank_prompt = good();
    blank_prompt[0].cases[0].prompt = "  ".into();
    for (label, suites) in [
        ("dup ids", dup_ids.to_vec()),
        ("empty", empty.to_vec()),
        ("dup names", dup_names.to_vec()),
        ("bad file", bad_file.to_vec()),
        ("many files", many_files.to_vec()),
        ("big file", big_file.to_vec()),
        ("two primary", two_primary.to_vec()),
        ("no primary", no_primary.to_vec()),
        ("blank prompt", blank_prompt.to_vec()),
        ("none", vec![]),
    ] {
        assert!(
            evaluate(&rules(), &suites, &baseline, &candidate, &|| false, never).is_err(),
            "{label} must be rejected"
        );
    }

    for bad in [
        Rules {
            max_regression: f64::NAN,
            ..rules()
        },
        Rules {
            min_improvement: f64::INFINITY,
            ..rules()
        },
        Rules {
            max_regression: -1.0,
            ..rules()
        },
        Rules {
            alpha: 0.0,
            ..rules()
        },
        Rules {
            alpha: 1.5,
            ..rules()
        },
    ] {
        assert!(evaluate(&bad, &good(), &baseline, &candidate, &|| false, never).is_err());
    }

    // Lineage and status.
    let stranger = Version {
        parent: Some(9),
        ..candidate.clone()
    };
    let same = Version {
        id: 1,
        parent: Some(1),
        ..candidate.clone()
    };
    let active = Version {
        status: "active".into(),
        ..candidate.clone()
    };
    for v in [&stranger, &same, &active, &baseline] {
        assert!(evaluate(&rules(), &good(), &baseline, v, &|| false, never).is_err());
    }
}

#[test]
fn interleaves_baseline_and_candidate_per_case() {
    let (baseline, candidate) = versions();
    let suites = [suite("primary", true, 3), suite("guard", false, 2)];
    let mut order = Vec::new();
    evaluate(
        &rules(),
        &suites,
        &baseline,
        &candidate,
        &|| false,
        |v, c| {
            order.push((v.id, c.id.clone()));
            Ok(true)
        },
    )
    .unwrap();
    let expected: Vec<(i64, String)> = ["primary0", "primary1", "primary2", "guard0", "guard1"]
        .iter()
        .flat_map(|id| [(1, id.to_string()), (2, id.to_string())])
        .collect();
    assert_eq!(order, expected);
}

#[test]
fn net_wins_and_significance_gate_the_primary() {
    let (baseline, candidate) = versions();
    let suites = [suite("primary", true, 20), suite("guard", false, 2)];
    let gate = Rules {
        alpha: 0.1,
        ..rules()
    };
    // 5 candidate-only wins, no losses: p = 1/32.
    let report = evaluate(
        &gate,
        &suites,
        &baseline,
        &candidate,
        &|| false,
        runner(10, 15, "primary"),
    )
    .unwrap();
    assert!(report.passed, "{}", report.reason);
    assert_eq!(
        (
            report.scores[0].candidate_only,
            report.scores[0].baseline_only
        ),
        (5, 0)
    );
    // 3 wins vs 1 loss (baseline solves index 5 only, candidate solves 0..5 minus nothing else).
    let mixed = |v: &Version, c: &Case| -> Result<bool> {
        let Some(i) =
            c.id.strip_prefix("primary")
                .and_then(|i| i.parse::<usize>().ok())
        else {
            return Ok(true);
        };
        Ok(if v.id == 2 {
            (10..13).contains(&i) || (0..9).contains(&i)
        } else {
            (0..9).contains(&i) && i != 0 || i == 9
        })
    };
    // candidate: 0..9 + 10..13 = 12 solved; baseline: 1..9 + 9 = 1..=9 -> 9 solved.
    let report = evaluate(&gate, &suites, &baseline, &candidate, &|| false, mixed).unwrap();
    assert_eq!(
        (
            report.scores[0].candidate_only,
            report.scores[0].baseline_only
        ),
        (4, 1)
    );
    assert!(!report.passed);
    assert!(report.reason.contains("significant"), "{}", report.reason);
    // Same data, legacy rules pass: significance is opt-in.
    assert!(
        evaluate(&rules(), &suites, &baseline, &candidate, &|| false, mixed)
            .unwrap()
            .passed
    );
    // The sign test boundary is inclusive.
    let exact = Rules {
        alpha: 1.0 / 32.0,
        ..rules()
    };
    assert!(
        evaluate(
            &exact,
            &suites,
            &baseline,
            &candidate,
            &|| false,
            runner(10, 15, "primary")
        )
        .unwrap()
        .passed
    );
    // min_net_wins is checked independently of alpha.
    let need = Rules {
        min_net_wins: 6,
        ..rules()
    };
    let report = evaluate(
        &need,
        &suites,
        &baseline,
        &candidate,
        &|| false,
        runner(10, 15, "primary"),
    )
    .unwrap();
    assert!(
        !report.passed && report.reason.contains("net"),
        "{}",
        report.reason
    );
    let enough = Rules {
        min_net_wins: 5,
        ..rules()
    };
    assert!(
        evaluate(
            &enough,
            &suites,
            &baseline,
            &candidate,
            &|| false,
            runner(10, 15, "primary")
        )
        .unwrap()
        .passed
    );
    // An insufficient point gain keeps the legacy reason regardless of the other criteria.
    let big = Rules {
        min_improvement: 30.0,
        alpha: 0.1,
        ..rules()
    };
    let report = evaluate(
        &big,
        &suites,
        &baseline,
        &candidate,
        &|| false,
        runner(10, 15, "primary"),
    )
    .unwrap();
    assert!(!report.passed && report.reason.contains("required improvement"));
}

#[test]
fn check_cases_verify_files_in_the_seeded_workspace() {
    let (baseline, candidate) = versions();
    let mut seeded = Files::new();
    seeded.insert("data/in.txt".into(), "alpha".into());
    let file_case = Case {
        id: "file".into(),
        prompt: "uppercase the file".into(),
        files: seeded,
        check: Check::All {
            checks: vec![
                Check::FileEquals {
                    path: "data/in.txt".into(),
                    content: "ALPHA".into(),
                },
                Check::AnswerContains {
                    needle: "done".into(),
                },
            ],
        },
    };
    let primary = Suite {
        name: "primary".into(),
        primary: true,
        cases: vec![file_case.clone()],
    };
    let guard = suite("guard", false, 1);
    // The "agent" under the candidate rewrites the file and says done; the baseline does nothing.
    let run = |v: &Version, c: &Case| -> Result<bool> {
        let dir = tempfile::tempdir()?;
        for (path, content) in &c.files {
            let full = dir.path().join(path);
            fs::create_dir_all(full.parent().unwrap())?;
            fs::write(full, content)?;
        }
        let answer = if c.id == "file" && v.id == 2 {
            fs::write(dir.path().join("data/in.txt"), "ALPHA\n")?;
            "done"
        } else {
            "yes"
        };
        Ok(c.check.verify(dir.path(), answer))
    };
    let report = evaluate(
        &rules(),
        &[primary, guard],
        &baseline,
        &candidate,
        &|| false,
        run,
    )
    .unwrap();
    assert!(report.passed, "{}", report.reason);
    assert_eq!(
        (report.scores[0].baseline, report.scores[0].candidate),
        (0.0, 100.0)
    );
}

fn write_suites(dir: &TempDir, primary: &str, guard: &str) -> GateConfig {
    let p = dir.path().join("primary.jsonl");
    let g = dir.path().join("guard.jsonl");
    fs::write(&p, primary).unwrap();
    fs::write(&g, guard).unwrap();
    GateConfig {
        suites: vec![
            SuiteConfig {
                name: "primary".into(),
                path: p,
                primary: true,
            },
            SuiteConfig {
                name: "guard".into(),
                path: g,
                primary: false,
            },
        ],
        max_regression: 0.0,
        min_improvement: 1.0,
    }
}

#[test]
fn manual_suites_accept_checks_and_legacy_expectations() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_suites(
        &dir,
        "{\"id\":\"a\",\"prompt\":\"private primary\",\"expected\":{\"ok\":true},\"matcher\":\"json\"}\n\
             {\"id\":\"b\",\"prompt\":\"private file\",\"files\":{\"x.txt\":\"1\"},\"check\":{\"kind\":\"file_equals\",\"path\":\"x.txt\",\"content\":\"2\"}}\n",
        "{\"id\":\"c\",\"prompt\":\"private guard\",\"expected\":\"yes\"}\n",
    );
    let suites = load_manual(&config).unwrap();
    assert_eq!(
        suites[0].cases[0].check,
        Check::AnswerJson {
            expected: serde_json::json!({"ok": true})
        }
    );
    assert_eq!(
        suites[0].cases[1].check,
        Check::FileEquals {
            path: "x.txt".into(),
            content: "2".into()
        }
    );
    assert_eq!(suites[0].cases[1].files["x.txt"], "1");
    assert_eq!(
        suites[1].cases[0].check,
        Check::AnswerExact {
            expected: "yes".into()
        }
    );
    let prompts = held_out_prompts(&suites);
    assert_eq!(prompts.len(), 3);
    assert!(prompts.contains("private guard"));
}

#[test]
fn whole_output_json_must_match() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_suites(
        &dir,
        "{\"id\":\"a\",\"prompt\":\"p\",\"expected\":{\"ok\":true},\"matcher\":\"json\"}\n",
        "{\"id\":\"b\",\"prompt\":\"g\",\"expected\":\"yes\"}\n",
    );
    let suites = load_manual(&config).unwrap();
    let check = &suites[0].cases[0].check;
    let ws = tempfile::tempdir().unwrap();
    assert!(check.verify(ws.path(), "{\"ok\":true}"));
    assert!(!check.verify(ws.path(), "{\"ok\":true} trailing"));
    assert!(suites[1].cases[0].check.verify(ws.path(), " yes \n"));
}

#[test]
fn manual_suites_reject_invalid_configuration_without_leaking() {
    let dir = tempfile::tempdir().unwrap();
    let exact = "{\"id\":\"b\",\"prompt\":\"g\",\"expected\":\"yes\"}\n";
    let base = "{\"id\":\"a\",\"prompt\":\"SECRET p\",\"expected\":\"yes\"}\n";
    let bad_cases = [
        ("neither", "{\"id\":\"a\",\"prompt\":\"SECRET p\"}\n"),
        (
            "both",
            "{\"id\":\"a\",\"prompt\":\"SECRET p\",\"expected\":\"y\",\"check\":{\"kind\":\"answer_exact\",\"expected\":\"y\"}}\n",
        ),
        (
            "matcher with check",
            "{\"id\":\"a\",\"prompt\":\"SECRET p\",\"matcher\":\"exact\",\"check\":{\"kind\":\"answer_exact\",\"expected\":\"y\"}}\n",
        ),
        (
            "exact non-string",
            "{\"id\":\"a\",\"prompt\":\"SECRET p\",\"expected\":3}\n",
        ),
        (
            "unknown field",
            "{\"id\":\"a\",\"prompt\":\"SECRET p\",\"expected\":\"y\",\"extra\":1}\n",
        ),
        (
            "bad check",
            "{\"id\":\"a\",\"prompt\":\"SECRET p\",\"check\":{\"kind\":\"nope\"}}\n",
        ),
        ("blank line", "\n"),
        ("duplicate id", &format!("{base}{base}")),
        (
            "empty prompt",
            "{\"id\":\"a\",\"prompt\":\" \",\"expected\":\"y\"}\n",
        ),
        (
            "absolute file",
            "{\"id\":\"a\",\"prompt\":\"SECRET p\",\"expected\":\"y\",\"files\":{\"/etc/x\":\"\"}}\n",
        ),
        (
            "dotdot file",
            "{\"id\":\"a\",\"prompt\":\"SECRET p\",\"expected\":\"y\",\"files\":{\"a/../../x\":\"\"}}\n",
        ),
        ("empty suite", ""),
    ];
    for (label, text) in bad_cases {
        let error = load_manual(&write_suites(&dir, text, exact)).unwrap_err();
        assert!(!format!("{error:#}").contains("SECRET"), "{label} leaked");
    }
    // Shared prompts across suites.
    let shared = "{\"id\":\"z\",\"prompt\":\"  SECRET p \",\"expected\":\"yes\"}\n";
    assert!(load_manual(&write_suites(&dir, base, shared)).is_err());
    // Config-level checks.
    let mut config = write_suites(&dir, base, exact);
    assert!(load_manual(&config).is_ok());
    config.max_regression = f64::NAN;
    assert!(load_manual(&config).is_err());
    config.max_regression = 0.0;
    config.min_improvement = 0.0;
    assert!(load_manual(&config).is_err());
    config.min_improvement = 1.0;
    config.suites[1].primary = true;
    assert!(load_manual(&config).is_err());
    config.suites[1].primary = false;
    config.suites[1].name = "primary".into();
    assert!(load_manual(&config).is_err());
    config.suites[1].name = "guard".into();
    config.suites.truncate(1);
    assert!(load_manual(&config).is_err());
}

#[test]
fn training_overlap_is_rejected_without_disclosing_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let prompts: HashSet<String> = ["private primary".to_owned()].into();
    let train = dir.path().join("train.jsonl");
    let valid = dir.path().join("valid.jsonl");
    fs::write(
        &train,
        "{\"messages\":[{\"role\":\"user\",\"content\":\"  private primary  \"}]}\n",
    )
    .unwrap();
    fs::write(
        &valid,
        "{\"messages\":[{\"role\":\"user\",\"content\":\"unseen task\"}]}\n",
    )
    .unwrap();
    let error = check_training_isolation(&prompts, &train, &valid).unwrap_err();
    assert!(!error.to_string().contains("private primary"));
    fs::write(&train, "{\"messages\":[{\"role\":\"user\",\"content\":\"different task\"},{\"role\":\"assistant\",\"content\":null,\"tool_calls\":[]},{\"role\":\"tool\",\"content\":\"private primary\"}]}\n").unwrap();
    check_training_isolation(&prompts, &train, &valid).unwrap();
    // Overlap in the validation split is caught too, and malformed records are errors.
    fs::write(
        &valid,
        "{\"messages\":[{\"role\":\"user\",\"content\":\"private primary\"}]}\n",
    )
    .unwrap();
    assert!(check_training_isolation(&prompts, &train, &valid).is_err());
    fs::write(&valid, "{\"messages\":[{\"role\":\"user\"}]}\n").unwrap();
    assert!(check_training_isolation(&prompts, &train, &valid).is_err());
}

/// The early stop only skips cases that cannot matter: for every outcome pattern and rule set,
/// admission is identical with and without it, and it never runs more cases.
#[test]
fn stopping_when_futile_never_changes_admission() {
    let (baseline, candidate) = versions();
    let suites = [suite("primary", true, 5), suite("guard", false, 2)];
    let mut stopped_somewhere = false;
    for pattern in 0..4usize.pow(7) {
        let outcome = |v: &Version, c: &Case| -> Result<bool> {
            let index = if let Some(i) = c.id.strip_prefix("primary") {
                i.parse::<usize>().unwrap()
            } else {
                5 + c
                    .id
                    .strip_prefix("guard")
                    .unwrap()
                    .parse::<usize>()
                    .unwrap()
            };
            let pair = pattern / 4usize.pow(index as u32) % 4;
            Ok(if v.id == 1 {
                pair & 1 == 1
            } else {
                pair & 2 == 2
            })
        };
        for (min_improvement, min_net_wins, alpha) in [
            (1.0, 0, 1.0),
            (40.0, 0, 1.0),
            (1.0, 2, 1.0),
            (1.0, 0, 0.2),
            (20.0, 3, 0.1),
        ] {
            let base = Rules {
                min_improvement,
                min_net_wins,
                alpha,
                ..rules()
            };
            let (mut full_calls, mut early_calls) = (0, 0);
            let full = evaluate(&base, &suites, &baseline, &candidate, &|| false, |v, c| {
                full_calls += 1;
                outcome(v, c)
            })
            .unwrap();
            let early = evaluate(
                &Rules {
                    stop_when_futile: true,
                    ..base
                },
                &suites,
                &baseline,
                &candidate,
                &|| false,
                |v, c| {
                    early_calls += 1;
                    outcome(v, c)
                },
            )
            .unwrap();
            assert_eq!(
                full.passed, early.passed,
                "pattern {pattern} rules {min_improvement}/{min_net_wins}/{alpha}"
            );
            assert!(early_calls <= full_calls);
            if early.passed {
                assert_eq!(
                    early_calls, full_calls,
                    "an accepted candidate is always fully evaluated"
                );
                assert_eq!(full.reason, early.reason);
            }
            stopped_somewhere |= early_calls < full_calls;
        }
    }
    assert!(stopped_somewhere, "the early stop never triggered");
}

#[test]
fn a_hopeless_primary_suite_skips_its_remaining_cases_and_says_so() {
    let (baseline, candidate) = versions();
    let suites = [suite("primary", true, 20), suite("guard", false, 20)];
    let mut calls = 0;
    let report = evaluate(
        &Rules {
            min_improvement: 50.0,
            stop_when_futile: true,
            ..rules()
        },
        &suites,
        &baseline,
        &candidate,
        &|| false,
        |_, _| {
            calls += 1;
            Ok(true)
        },
    )
    .unwrap();
    assert!(
        !report.passed && report.reason.contains("stopped early"),
        "{}",
        report.reason
    );
    assert_eq!(
        calls, 22,
        "stops once 11 of 20 cases are done (best case can no longer gain 50 points); a full run is 80 calls"
    );
    assert_eq!(report.scores.len(), 1, "the guard suite was skipped");
}
