use super::*;

#[test]
fn diff_rewrite_strips_context_prefix_space() {
    let lines = ["-old", "+new", " keep"].map(str::to_owned);
    assert_eq!(normalize_block(&lines, true), "new\nkeep");
}

#[test]
fn replace_and_insert_bodies_for_one_find_are_rejected() {
    let input = "*** Edit File: a.txt\n*** Find\nold\n*** Replace\nnew\n*** Insert After\nextra";
    let Err(error) = split_sloppy_sections(input) else {
        panic!("conflicting bodies must not compile");
    };
    assert!(
        error
            .to_string()
            .contains("both *** Replace and *** Insert After bodies"),
        "{error}"
    );
}
