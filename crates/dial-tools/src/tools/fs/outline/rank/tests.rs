use super::*;

#[test]
fn declared_names_follow_declaration_keywords() {
    assert_eq!(declared_name("pub struct Config {"), Some("Config"));
    assert_eq!(
        declared_name("pub(crate) fn load(path: &Path)"),
        Some("load")
    );
    assert_eq!(declared_name("export default class Store {"), Some("Store"));
    assert_eq!(declared_name("def parse(text):"), Some("parse"));
    assert_eq!(declared_name("impl Display for Config {"), None);
    assert_eq!(declared_name("#[cfg(test)]"), None);
    assert_eq!(
        declared_name("mod system;"),
        None,
        "a child module, not a definition"
    );
    assert_eq!(declared_name("// let me explain"), None);
    assert_eq!(
        declared_name("This function returns the parsed value"),
        None
    );
    assert_eq!(
        declared_name("pub const fn limit() -> usize"),
        Some("limit")
    );
    assert_eq!(
        declared_name("export async function loadUser() {"),
        Some("loadUser")
    );
}

#[test]
fn files_the_others_reference_rank_first() {
    let files = [
        (
            vec!["main_loop"],
            "fn main_loop() { let c = Config::load(); store_value(c); }",
        ),
        (vec!["Config"], "pub struct Config {}"),
        (vec!["store_value"], "fn store_value(c: Config) {}"),
        (vec!["unused_helper"], "fn unused_helper() {}"),
    ];
    let rank = ranks(&files);
    assert!((rank.iter().sum::<f64>() - 1.0).abs() < 1e-9, "{rank:?}");
    // Config is referenced by two files, store_value by one, the others by none.
    assert!(rank[1] > rank[2], "{rank:?}");
    assert!(rank[2] > rank[0], "{rank:?}");
    assert!((rank[0] - rank[3]).abs() < 1e-9, "{rank:?}");
}

#[test]
fn unconnected_files_share_rank_and_common_names_are_ignored() {
    // `run_job` is declared in more than five files, so its mentions link nothing; `write` is an
    // ordinary word, so declaring it links nothing either.
    let mut files: Vec<(Vec<&str>, &str)> = (0..6)
        .map(|_| (vec!["run_job"], "fn run_job() { run_job() }"))
        .collect();
    files.push((vec!["write"], "fn write() { run_job(); run_job(); }"));
    files.push((vec!["caller_fn"], "fn caller_fn() { write(); }"));
    let rank = ranks(&files);
    let first = rank[0];
    assert!(rank.iter().all(|r| (r - first).abs() < 1e-9), "{rank:?}");
    assert!(ranks(&[]).is_empty());
}
