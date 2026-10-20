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
    ];
    let hint = match_hint(&skills, "Please fill these PDF forms for me").unwrap();
    assert!(hint.contains("pdf-forms may match"), "{hint}");
    assert!(!hint.contains("release"), "{hint}");
    assert!(hint.contains("`skill` tool"));
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
        "Skill hint: review may match this request. Load the matching skill with the `skill` tool before covered work."
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
    assert!(match_hint(&skills, "remove the dead code here").is_some());
}
