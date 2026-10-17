use super::*;

fn src(url: &str, reference: Option<&str>, path: Option<&str>) -> Source {
    Source {
        url: url.into(),
        reference: reference.map(Into::into),
        path: path.map(Into::into),
    }
}

#[test]
fn parses_url_ref_and_path() {
    assert_eq!(
        parse("https://example.com/org/repo.git@v1.2#skills/pdf").unwrap(),
        src(
            "https://example.com/org/repo.git",
            Some("v1.2"),
            Some("skills/pdf")
        )
    );
    assert_eq!(
        parse("https://example.com/org/repo").unwrap(),
        src("https://example.com/org/repo", None, None)
    );
    assert_eq!(
        parse("https://example.com/org/repo#pdf").unwrap(),
        src("https://example.com/org/repo", None, Some("pdf"))
    );
}

#[test]
fn keeps_scp_user_and_file_scheme_locations_intact() {
    assert_eq!(
        parse("git@github.com:owner/repo.git@main").unwrap(),
        src("git@github.com:owner/repo.git", Some("main"), None)
    );
    assert_eq!(
        parse("git@github.com:owner/repo.git").unwrap(),
        src("git@github.com:owner/repo.git", None, None)
    );
    assert_eq!(
        parse("file:///tmp/r@v1").unwrap(),
        src("file:///tmp/r", Some("v1"), None)
    );
    assert_eq!(
        parse("https://user:pw@host/r@v1").unwrap(),
        src("https://user:pw@host/r", Some("v1"), None)
    );
}

#[test]
fn local_paths_split_refs_without_touching_the_path() {
    assert_eq!(
        parse("/tmp/repo@dev").unwrap(),
        src("/tmp/repo", Some("dev"), None)
    );
    assert_eq!(parse("./repo").unwrap(), src("./repo", None, None));
    assert_eq!(
        parse("../repo#skills").unwrap(),
        src("../repo", None, Some("skills"))
    );
}

#[test]
fn rejects_unsafe_or_malformed_sources() {
    assert!(parse("").is_err());
    assert!(parse("@v1").is_err());
    assert!(parse("https://x/r@").is_err());
    assert!(parse("https://x/r#").is_err());
    assert!(parse("https://x/r#../secret").is_err());
    assert!(parse("https://x/r#/etc").is_err());
    assert!(parse("https://x/r#a\\b").is_err());
    assert!(parse("--upload-pack=evil").is_err());
    assert!(parse("https://x/r@--evil").is_err());
}

#[test]
fn current_dir_components_are_allowed_in_paths() {
    assert!(validate_relative_path("./skills/pdf").is_ok());
    assert!(validate_relative_path("skills/../x").is_err());
}

#[test]
fn derives_package_names() {
    assert_eq!(
        default_name(&src("https://x/org/repo.git", None, None)).as_deref(),
        Some("repo")
    );
    assert_eq!(
        default_name(&src("git@h:o/tools.git", None, None)).as_deref(),
        Some("tools")
    );
    assert_eq!(
        default_name(&src("https://x/r", None, Some("skills/pdf/"))).as_deref(),
        Some("pdf")
    );
    assert_eq!(default_name(&src("https://x/r", None, Some("."))), None);
}

#[test]
fn package_name_rules() {
    assert!(valid_package_name("pdf-tools_2.0"));
    assert!(!valid_package_name(".hidden"));
    assert!(!valid_package_name("a b"));
    assert!(!valid_package_name(&"x".repeat(65)));
    assert!(!valid_package_name(""));
}
