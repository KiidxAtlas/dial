use super::*;
#[test]
fn commands_parse_citations_without_reopening_paths() {
    let (tool, args) = parse("compare artifact://a artifact://b").unwrap().unwrap();
    assert_eq!(tool, "investigate");
    assert_eq!(args["reference"], "artifact://a");
    assert_eq!(parse("inspect").unwrap(), None);
    assert_eq!(parse("compare a").unwrap(), None);
    let (tool, args) = parse("correct artifact://a   this happened later")
        .unwrap()
        .unwrap();
    assert_eq!(tool, "workbench");
    assert_eq!(args["correction"], "this happened later");
    assert!(parse("fetch https://example.test").is_err());
}
