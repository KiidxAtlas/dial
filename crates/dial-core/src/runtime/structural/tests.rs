use super::*;

const RUST: &str = "\
trait Store {
    fn load(&self) -> Result<u8>;
}

fn validate(messages: &[u8], limit: usize) -> Result<()> {
    ensure!(messages.len() < limit, \"too many {}\", tool.name(&x));
    Ok(())
}
";

fn hits(lang: &str, pattern: &str, src: &str) -> Vec<Hit> {
    Query::compile(parse_lang(lang).unwrap(), pattern, None)
        .unwrap()
        .find(src, 10)
        .unwrap()
}

#[test]
fn signature_patterns_match_definitions_and_bodiless_declarations() {
    let found = hits("rust", "fn $NAME($$$ARGS) -> Result<$T>", RUST);
    let lines: Vec<usize> = found.iter().map(|hit| hit.line).collect();
    assert_eq!(lines, [2, 5]);
    let validate = &found[1];
    assert!(
        validate
            .captures
            .contains(&("$NAME".into(), "validate".into()))
    );
    assert!(
        validate
            .captures
            .contains(&("$$$ARGS".into(), "messages: &[u8], limit: usize".into()))
    );
}

#[test]
fn completed_signatures_keep_their_visibility_requirement() {
    let src = "pub fn open(path: &str) -> Result<File> { todo!() }\nfn close() -> Result<()> { Ok(()) }\n";
    let found = hits("rust", "pub fn $NAME($$$ARGS) -> Result<$T>", src);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].line, 1);
}

#[test]
fn signature_completion_respects_the_limit_and_reports_each_node_once() {
    let query = Query::compile(parse_lang("rust").unwrap(), "fn $N($$$) -> $T", None).unwrap();
    assert_eq!(query.find(RUST, 1).unwrap().len(), 1);
    assert_eq!(query.find(RUST, 10).unwrap().len(), 2);
}

#[test]
fn bodiless_patterns_invalid_without_a_body_search_the_completed_form() {
    let src = "class A { public int size(int x) { return x; } }";
    let found = hits("java", "public $T $N($$$A)", src);
    assert_eq!(found.len(), 1);
    assert!(found[0].captures.contains(&("$N".into(), "size".into())));
}

#[test]
fn non_declaration_patterns_are_not_completed() {
    let lang = parse_lang("rust").unwrap();
    assert!(completed_declaration("$X", lang).is_none());
    assert!(completed_declaration("foo($A)", lang).is_none());
    assert!(completed_declaration("fn $N() {}", lang).is_none());
}

#[test]
fn rewrites_never_use_the_completed_form() {
    let query = Query::compile(
        parse_lang("rust").unwrap(),
        "fn $N($$$A) -> Result<$T>",
        Some("fn $N($$$A) -> Option<$T>"),
    )
    .unwrap();
    let rewritten = query.rewrite(RUST).unwrap().unwrap();
    assert!(
        rewritten
            .text
            .contains("fn validate(messages: &[u8], limit: usize) -> Result<()> {")
    );
}

#[test]
fn multi_captures_keep_original_source_text() {
    let found = hits("rust", "ensure!($$$A)", RUST);
    assert_eq!(found.len(), 1);
    assert_eq!(
        found[0].captures,
        [(
            "$$$A".to_owned(),
            "messages.len() < limit, \"too many {}\", tool.name(&x)".to_owned()
        )]
    );
}
