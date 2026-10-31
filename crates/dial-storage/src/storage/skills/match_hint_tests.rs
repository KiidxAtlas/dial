use super::*;

fn skill(name: &str, description: &str) -> Skill {
    Skill {
        name: name.into(),
        description: description.into(),
        path: PathBuf::from(format!("/x/{name}/SKILL.md")),
        source: Scope::User,
        origin: "Dial",
    }
}

#[test]
fn names_skills_whose_name_or_description_shares_words_with_the_request() {
    let skills = [
        skill("pdf-forms", "Fill PDF forms and extract fields"),
        skill("release", "Cut a versioned release"),
        skill("tables", "Extract fields from scanned forms into tables"),
    ];
    let hint = match_hint(&skills, "Please fill these PDF forms for me").unwrap();
    assert!(hint.contains("pdf-forms may match"), "{hint}");
    assert!(!hint.contains("release"), "{hint}");
    assert!(hint.contains("`skill` tool"));
    // Three shared description words suggest a skill the request does not name.
    let hint = match_hint(&skills, "extract the fields from these scanned invoices").unwrap();
    assert!(hint.contains("tables may match"), "{hint}");
}

#[test]
fn ordinary_prompts_get_no_hint() {
    let skills = [skill("pdf-forms", "Fill PDF forms")];
    assert_eq!(match_hint(&skills, "Summarize the readme"), None);
    assert_eq!(match_hint(&[], "fill pdf forms"), None);
}

#[test]
fn short_and_stop_words_never_match_on_their_own() {
    let skills = [skill("review", "Review this and that with care")];
    assert_eq!(match_hint(&skills, "this is with that"), None);
    assert_eq!(match_hint(&skills, "review the diff"), Some(
        "Skill hint: review may match this request. If a matching skill covers substantial multi-step work, load it with the `skill` tool in the same response as your first reads, not as a round of its own."
            .into()
    ));
}

#[test]
fn one_generic_description_word_or_pasted_code_is_not_a_match() {
    let skills = [skill(
        "code-cleanup",
        "Audit or clean up dead code and duplicated behavior",
    )];
    assert_eq!(match_hint(&skills, "why does my code crash"), None);
    assert_eq!(
        match_hint(
            &skills,
            "why does this crash\n    dead_code = cleanup(code)"
        ),
        None
    );
    assert!(match_hint(&skills, "remove the dead code and duplicated helpers here").is_some());
    assert!(match_hint(&skills, "remove the dead code here").is_none());
}

#[test]
fn an_ordinary_bug_report_does_not_suggest_a_debugging_skill_it_does_not_name() {
    let skills = [skill(
        "debugging",
        "Diagnose failures, wrong results, crashes or intermittent behavior; implement and verify the causal fix when requested.",
    )];
    // Shares only "verify" and "behavior" with the description: a small fix needs no skill.
    let request = "Customers receive incorrect invoice totals when quantities differ from one. Fix the production invoice calculation, preserve its public API and the integer-cents representation, and verify the behavior.";
    assert_eq!(match_hint(&skills, request), None);
    // Shares "wrong", "verify" and "behavior": three words, but all of them describe coding work
    // in general, not the debugging skill's domain.
    let request = "Checkout applies the wrong discount. Fix amount_due using current source declarations and verify its public behavior. Preserve the public API and current settings. Historical notes are advisory and may be stale.";
    assert_eq!(match_hint(&skills, request), None);
    assert!(match_hint(&skills, "start debugging this").is_some());
    // Words of the skill's own domain still suggest it.
    let hint = match_hint(
        &skills,
        "Diagnose the intermittent crashes in the worker pool",
    )
    .unwrap();
    assert!(hint.contains("debugging may match"), "{hint}");
}

#[test]
fn task_vocabulary_alone_never_suggests_a_skill() {
    let skills = [skill(
        "checks",
        "Verify changes: run tests, check results and report wrong behavior or errors",
    )];
    assert_eq!(
        match_hint(
            &skills,
            "Verify the changes, run the tests, check the results and fix the wrong behavior and errors"
        ),
        None
    );
    // The skill's name is still a match on its own.
    assert!(match_hint(&skills, "run the checks").is_some());
}
