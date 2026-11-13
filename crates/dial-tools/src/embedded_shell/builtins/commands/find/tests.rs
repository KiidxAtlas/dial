use super::*;

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|arg| (*arg).to_owned()).collect()
}

#[test]
fn plan_defaults_to_printing_matches_under_dot() {
    let plan = Plan::parse(&args(&[])).unwrap();
    assert_eq!(plan.root, ".");
    assert_eq!(plan.actions, vec![Action::Print]);
    assert_eq!(Plan::parse(&args(&["src"])).unwrap().root, "src");
}

#[test]
fn plan_parses_exec_terminators_and_print_actions() {
    let plan = Plan::parse(&args(&[
        "root", "-name", "*.rs", "-exec", "cat", "{}", ";", "-exec", "echo", "{}", "+",
    ]))
    .unwrap();
    assert_eq!(plan.name.as_deref(), Some("*.rs"));
    assert_eq!(
        plan.actions,
        vec![
            Action::Exec {
                argv: args(&["cat", "{}"]),
                batched: false,
            },
            Action::Exec {
                argv: args(&["echo"]),
                batched: true,
            },
        ]
    );
    let print0 = Plan::parse(&args(&["-print0"])).unwrap();
    assert_eq!(print0.actions, vec![Action::Print0]);
}

#[test]
fn plan_rejects_malformed_expressions() {
    let cases: &[&[&str]] = &[
        &["-exec"],
        &["-exec", "cat", "{}"],
        &["-exec", ";"],
        &["-exec", "{}", "+"],
        &["-type", "x"],
        &["-maxdepth", "deep"],
        &["-name"],
        &["-bogus"],
        &["!", "-name", "x"],
    ];
    for case in cases {
        assert!(Plan::parse(&args(case)).is_err(), "args {case:?}");
    }
}

#[test]
fn plan_rejects_repeated_tests_instead_of_overwriting_them() {
    let cases: &[&[&str]] = &[
        &["-name", "*.rs", "-name", "*.md"],
        &["-type", "f", "-type", "d"],
        &["-maxdepth", "1", "-maxdepth", "2"],
    ];
    for case in cases {
        assert!(Plan::parse(&args(case)).is_err(), "args {case:?}");
    }
}

#[test]
fn plan_rejects_tests_after_actions() {
    // Tests apply to every match, so a test that follows an action would be misapplied.
    let cases: &[&[&str]] = &[
        &["-print", "-name", "x"],
        &["-exec", "true", ";", "-type", "f"],
    ];
    for case in cases {
        assert!(Plan::parse(&args(case)).is_err(), "args {case:?}");
    }
}

#[test]
fn plan_allows_braces_only_as_the_final_plus_argument() {
    assert!(Plan::parse(&args(&["-exec", "cp", "{}", "/x/{}", "{}", "+"])).is_err());
    assert!(Plan::parse(&args(&["-exec", "cp", "{}", "/x/{}", "{}", ";"])).is_ok());
}

#[test]
fn arg_cost_counts_encoded_units_per_platform() {
    assert_eq!(arg_cost("abc", false), 4, "bytes plus separator");
    assert_eq!(arg_cost("é", false), 3, "two UTF-8 bytes plus separator");
    assert_eq!(arg_cost("😀", false), 5, "four UTF-8 bytes plus separator");
    assert_eq!(
        arg_cost("abc", true),
        9,
        "units doubled plus quotes and separator"
    );
    assert_eq!(
        arg_cost("😀", true),
        7,
        "a surrogate pair is two UTF-16 units"
    );
}

#[test]
fn argv_cost_includes_the_template() {
    assert_eq!(argv_cost(&args(&["cat", "a b"]), false), 4 + 4);
}

#[test]
fn windows_budget_stays_below_the_command_line_limit() {
    assert!(command_line_budget(true) < 32_767);
    assert_eq!(command_line_budget(false), 64 * 1024);
}

#[test]
fn batch_stops_at_the_path_count_limit() {
    let mut batch = Batch::new(&args(&["cmd"]), false);
    for _ in 0..BATCH_MAX_PATHS {
        assert!(batch.fits(2));
        batch.push("a", 2);
    }
    assert!(!batch.fits(2));
    assert_eq!(batch.take().len(), BATCH_MAX_PATHS);
    assert!(batch.fits(2), "a taken batch starts empty again");
}

#[test]
fn batch_budget_counts_the_template_and_unicode_paths() {
    let template = args(&["cmd", "é"]);
    let windows = false;
    let mut batch = Batch::new(&template, windows);
    let budget = command_line_budget(windows);
    // Paths cost 100 each so the byte budget, not the 1000-path limit, ends the batch.
    let mut used = argv_cost(&template, windows);
    while used + 100 <= budget {
        assert!(batch.fits_alone(100));
        assert!(batch.fits(100));
        batch.push("some/path", 100);
        used += 100;
    }
    assert!(!batch.fits(100), "the next path would exceed the budget");
    assert!(
        batch.fits_alone(100),
        "a path alone still fits once a batch is flushed"
    );
    assert!(
        !batch.fits_alone(budget),
        "a path that cannot fit alone is rejected"
    );
}

#[cfg(windows)]
#[test]
fn windows_batches_stay_within_the_native_command_line() {
    let template = args(&["cmd", "/c", "echo"]);
    let mut batch = Batch::new(&template, true);
    let mut units: usize = template
        .iter()
        .map(|arg| arg.encode_utf16().count() + 1)
        .sum();
    while batch.fits(arg_cost("😀x", true)) {
        batch.push("😀x", arg_cost("😀x", true));
        units += "😀x".encode_utf16().count() + 1;
    }
    assert!(units <= 32_767, "command line of {units} units");
}
