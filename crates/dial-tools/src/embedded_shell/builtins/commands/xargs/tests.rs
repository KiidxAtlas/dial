use super::*;

fn words(list: &[&str]) -> Vec<String> {
    list.iter().map(|word| (*word).to_owned()).collect()
}

fn options(list: &[&str]) -> Vec<String> {
    words(list)
}

#[test]
fn split_quoted_follows_gnu_quoting() {
    let cases: &[(&str, &[&str])] = &[
        ("a b\tc\nd", &["a", "b", "c", "d"]),
        ("'a b' \"c d\" e\\ f", &["a b", "c d", "e f"]),
        (r#""x\"y\\z\q""#, &[r#"x"y\z\q"#]),
        ("''", &[""]),
        ("a\\", &["a"]),
        ("  \n ", &[]),
    ];
    for (input, expected) in cases {
        assert_eq!(
            split_quoted(input.as_bytes()).unwrap(),
            words(expected),
            "input {input:?}"
        );
    }
}

#[test]
fn split_quoted_rejects_unmatched_quotes() {
    assert!(split_quoted(b"'open").is_err());
    assert!(split_quoted(b"\"open").is_err());
}

#[test]
fn parse_options_reads_flags_up_to_the_command() {
    let parsed = parse_options(&options(&["-0r", "-n2", "-I{}", "cmd", "-x"]));
    let opts = parsed.unwrap();
    assert!(opts.null && opts.no_run_if_empty);
    assert_eq!(opts.per_call, Some(2));
    assert_eq!(opts.replace.as_deref(), Some("{}"));
    assert_eq!(opts.template, words(&["cmd", "-x"]));

    let separate = parse_options(&options(&["-n", "3", "-I", "%", "echo"])).unwrap();
    assert_eq!(separate.per_call, Some(3));
    assert_eq!(separate.replace.as_deref(), Some("%"));
}

#[test]
fn parse_options_defaults_to_echo_and_honours_double_dash() {
    assert_eq!(parse_options(&[]).unwrap().template, words(&["echo"]));
    let after_dashes = parse_options(&options(&["--", "-r"])).unwrap();
    assert!(!after_dashes.no_run_if_empty);
    assert_eq!(after_dashes.template, words(&["-r"]));
}

#[test]
fn parse_options_rejects_bad_values() {
    let bad: &[&[&str]] = &[&["-x"], &["-n", "0"], &["-n"], &["-n", "many"], &["-I", ""]];
    for args in bad {
        assert!(parse_options(&options(args)).is_err(), "args {args:?}");
    }
}

#[test]
fn step_maps_exit_codes_and_stops_like_gnu_xargs() {
    let cases: &[(Outcome, Step)] = &[
        (Outcome::Exited(0), Step::Next { failed: false }),
        (Outcome::Exited(1), Step::Next { failed: true }),
        // An actual exit status of 125 is an ordinary failure, not a stop.
        (Outcome::Exited(125), Step::Next { failed: true }),
        (Outcome::Exited(255), Step::Stop(124)),
        (Outcome::Exited(126), Step::Stop(126)),
        (Outcome::Exited(127), Step::Stop(127)),
        (Outcome::Stopped, Step::Stop(125)),
    ];
    for (outcome, expected) in cases {
        assert_eq!(step(*outcome), *expected, "outcome {outcome:?}");
    }
}

#[test]
fn invocations_batch_words_and_honour_empty_input_rules() {
    let base = |per_call, no_run| Options {
        per_call,
        no_run_if_empty: no_run,
        template: words(&["echo", "pre"]),
        ..Options::default()
    };
    assert_eq!(
        invocations(&base(Some(2), false), b"a b c").unwrap(),
        vec![
            words(&["echo", "pre", "a", "b"]),
            words(&["echo", "pre", "c"])
        ]
    );
    assert_eq!(
        invocations(&base(None, false), b"").unwrap(),
        vec![words(&["echo", "pre"])]
    );
    assert!(invocations(&base(None, true), b"").unwrap().is_empty());
}

#[test]
fn invocations_split_nul_records_without_quoting() {
    let opts = Options {
        null: true,
        template: words(&["echo"]),
        ..Options::default()
    };
    assert_eq!(
        invocations(&opts, b"a b\0'c'\0").unwrap(),
        vec![words(&["echo", "a b", "'c'"])]
    );
}

#[test]
fn invocations_replace_each_input_line() {
    let opts = Options {
        replace: Some("{}".to_owned()),
        template: words(&["echo", "[{}]", "{}"]),
        ..Options::default()
    };
    assert_eq!(
        invocations(&opts, b"a b\n\n  c\n").unwrap(),
        vec![
            words(&["echo", "[a b]", "a b"]),
            words(&["echo", "[c]", "c"]),
        ]
    );
    assert!(invocations(&opts, b"").unwrap().is_empty());
}
