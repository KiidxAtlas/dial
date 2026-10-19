use super::*;

fn parse(args: &str) -> CleanseArgs {
    parse_args(args).unwrap_or_else(|error| panic!("{args:?} failed: {error}"))
}

#[test]
fn empty_args_use_defaults() {
    assert_eq!(
        parse(""),
        CleanseArgs {
            request: None,
            all: false,
            include_tests: false,
            max_agents: DEFAULT_MAX_AGENTS,
            model: None,
        }
    );
}

#[test]
fn flags_combine_with_request_words() {
    let args = parse("fix  the --tests types -a -n 4");
    assert_eq!(args.request.as_deref(), Some("fix the types"));
    assert!(args.all);
    assert!(args.include_tests);
    assert_eq!(args.max_agents, 4);
    assert_eq!(args.model, None);
}

#[test]
fn short_and_long_flags_are_equivalent() {
    assert_eq!(parse("-t -a"), parse("--tests --all"));
    assert_eq!(parse("-n 2"), parse("--agents 2"));
}

#[test]
fn model_flag_takes_next_token() {
    let args = parse("-m @small");
    assert_eq!(args.model.as_deref(), Some("@small"));
    assert_eq!(args.request, None);
}

#[test]
fn negative_numbers_are_request_words() {
    assert_eq!(parse("-5 errors").request.as_deref(), Some("-5 errors"));
}

#[test]
fn rejects_bad_arguments() {
    for args in ["-n", "-n 0", "-n many", "-m", "--bogus", "-x", "fix -q"] {
        assert_eq!(parse_args(args), Err(CLEANSE_USAGE), "{args:?}");
    }
}
