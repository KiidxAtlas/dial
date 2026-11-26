use super::*;

const GOOD: &str = r#"{"version":1,"skills":[
        {"name":"pdf","description":"Read and fill PDF forms","url":"https://example.com/org/skills.git#pdf"},
        {"name":"docx","url":"git@example.com:org/skills.git@v2#docx"}
    ]}"#;

#[test]
fn parses_a_valid_index() {
    let entries = parse_index(GOOD.as_bytes()).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[1].description, "");
    assert_eq!(
        find(&entries, "docx").unwrap().url,
        "git@example.com:org/skills.git@v2#docx"
    );
    assert!(find(&entries, "missing").is_none());
}

#[test]
fn rejects_bad_indexes() {
    let cases = [
        r#"{"version":2,"skills":[]}"#,
        r#"{"version":1,"skills":[],"extra":1}"#,
        r#"{"version":1,"skills":[{"name":"a b","url":"https://x/r"}]}"#,
        r#"{"version":1,"skills":[{"name":"a","url":"https://x/r"},{"name":"a","url":"https://x/r"}]}"#,
        r#"{"version":1,"skills":[{"name":"a","url":"https://x/r#../up"}]}"#,
        r#"{"version":1,"skills":[{"name":"a","url":""}]}"#,
        "not json",
    ];
    for case in cases {
        assert!(parse_index(case.as_bytes()).is_err(), "accepted {case}");
    }
}

#[test]
fn search_matches_names_and_descriptions_case_insensitively() {
    let entries = parse_index(GOOD.as_bytes()).unwrap();
    assert_eq!(search(&entries, "PDF").len(), 1);
    assert_eq!(search(&entries, "forms")[0].name, "pdf");
    assert_eq!(search(&entries, "  ").len(), 2);
    assert!(search(&entries, "nothing-like-this").is_empty());
}

#[test]
fn loads_an_index_from_a_file_url_and_rejects_other_schemes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index #1%.json");
    fs::write(&path, GOOD).unwrap();
    let url = url::Url::from_file_path(&path).unwrap();
    let entries = fetch_index(url.as_str()).unwrap();
    assert_eq!(entries.len(), 2);
    assert!(fetch_index("ftp://example.com/index.json").is_err());
    assert!(local_index_path("file://other-host/share/index.json").is_err());
}
