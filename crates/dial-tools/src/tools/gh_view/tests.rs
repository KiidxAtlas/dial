use super::super::gh_common::test_support::FakeGh;
use super::*;
use serde_json::json;

fn workspace() -> (tempfile::TempDir, Workspace) {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    (dir, ws)
}

#[test]
fn repo_view_formats_fields_and_passes_branch() {
    let json = r#"{"nameWithOwner":"acme/widgets","description":"  Fast\twidgets  ","url":"https://github.com/acme/widgets","defaultBranchRef":{"name":"main"},"homepageUrl":"","forkCount":3,"isArchived":false,"isFork":false,"primaryLanguage":{"name":"Rust"},"repositoryTopics":[{"name":"cli"},{"topic":{"name":"tools"}}],"stargazerCount":42,"updatedAt":"2024-05-01T00:00:00Z","viewerPermission":"WRITE","visibility":"PUBLIC"}"#;
    let fake = FakeGh::new(json, "", 0);
    let (_dir, ws) = workspace();
    let out = run(
        "repo_view",
        &ws,
        &json!({"repo": "acme/widgets", "branch": "dev"}),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(!out.is_error);
    assert_eq!(
        out.text,
        "# acme/widgets\n\nFast    widgets\n\nURL: https://github.com/acme/widgets\nDefault branch: main\nBranch: dev\nVisibility: PUBLIC\nViewer permission: WRITE\nPrimary language: Rust\nStars: 42\nForks: 3\nArchived: false\nFork: false\nUpdated: 2024-05-01T00:00:00Z\nTopics: cli, tools"
    );
    let mut expected = strings(&["repo", "view", "acme/widgets", "--branch", "dev", "--json"]);
    expected.push(REPO_FIELDS.join(","));
    assert_eq!(fake.last_args(), expected);
}

#[test]
fn repo_view_without_description_uses_placeholder() {
    let fake = FakeGh::new(
        r#"{"nameWithOwner":"acme/widgets","description":null}"#,
        "",
        0,
    );
    let (_dir, ws) = workspace();
    let out = run(
        "repo_view",
        &ws,
        &json!({"repo": "acme/widgets"}),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(out.text, "# acme/widgets\n\nNo description provided.");
    let mut expected = strings(&["repo", "view", "acme/widgets", "--json"]);
    expected.push(REPO_FIELDS.join(","));
    assert_eq!(fake.last_args(), expected);
}

#[test]
fn file_read_decodes_base64_and_requests_ref() {
    let encoded = STANDARD.encode("hello\nworld\n");
    let wrapped = format!("{}\n{}", &encoded[..4], &encoded[4..]);
    let body = json!({
        "type": "file",
        "encoding": "base64",
        "size": 12,
        "content": wrapped,
        "html_url": "https://github.com/acme/widgets/blob/dev/src/a%20b.txt"
    })
    .to_string();
    let fake = FakeGh::new(&body, "", 0);
    let (_dir, ws) = workspace();
    let out = run(
        "file_read",
        &ws,
        &json!({"repo": "acme/widgets", "path": "src/a b.txt", "branch": "dev"}),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(out.text, "hello\nworld\n");
    assert_eq!(
        fake.last_args(),
        strings(&[
            "api",
            "/repos/acme/widgets/contents/src/a%20b.txt",
            "--method",
            "GET",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "Accept-Encoding: identity",
            "-f",
            "ref=dev",
        ])
    );
}

#[test]
fn file_read_reports_binary_content_without_dumping_it() {
    let body = json!({
        "type": "file",
        "encoding": "base64",
        "size": 3,
        "content": STANDARD.encode([0u8, 1, 2]),
        "html_url": "https://github.com/acme/widgets/blob/HEAD/bin.dat"
    })
    .to_string();
    let _fake = FakeGh::new(&body, "", 0);
    let (_dir, ws) = workspace();
    let out = run(
        "file_read",
        &ws,
        &json!({"repo": "acme/widgets", "path": "bin.dat"}),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(
        out.text,
        "[Cannot read binary file 'bin.dat' (3B); not valid UTF-8 text. Open https://github.com/acme/widgets/blob/HEAD/bin.dat to view it.]"
    );
}

#[test]
fn file_read_rejects_directories() {
    let _fake = FakeGh::new("[]", "", 0);
    let (_dir, ws) = workspace();
    let err = run(
        "file_read",
        &ws,
        &json!({"repo": "acme/widgets", "path": "src"}),
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert_eq!(err, "GitHub path 'src' is not a file.");
}

#[test]
fn file_read_explains_missing_bytes() {
    let body = json!({
        "type": "file",
        "encoding": "none",
        "size": 2000000,
        "html_url": "https://github.com/acme/widgets/blob/HEAD/big.bin"
    })
    .to_string();
    let _fake = FakeGh::new(&body, "", 0);
    let (_dir, ws) = workspace();
    let out = run(
        "file_read",
        &ws,
        &json!({"repo": "acme/widgets", "path": "big.bin"}),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(
        out.text,
        "[GitHub did not return file bytes for 'big.bin' (1.9MB). Open https://github.com/acme/widgets/blob/HEAD/big.bin to view it.]"
    );
}

#[test]
fn file_read_rejects_absolute_paths_before_calling_gh() {
    let (_dir, ws) = workspace();
    let err = run(
        "file_read",
        &ws,
        &json!({"repo": "acme/widgets", "path": "/etc/passwd"}),
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert_eq!(err, "path must be repository-relative");
}

#[test]
fn file_read_reports_image_metadata() {
    let mut png = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::new(3, 2))
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    let body = json!({
        "type": "file",
        "encoding": "base64",
        "size": png.len(),
        "content": STANDARD.encode(&png),
        "html_url": "https://github.com/acme/widgets/blob/HEAD/pic.png"
    })
    .to_string();
    let _fake = FakeGh::new(&body, "", 0);
    let (_dir, ws) = workspace();
    let out = run(
        "file_read",
        &ws,
        &json!({"repo": "acme/widgets", "path": "pic.png"}),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(
        out.text
            .starts_with("Image file: pic.png\nMIME: image/png\nSize: ")
    );
    assert!(out.text.contains("\nDimensions: 3x2\n"));
}

#[test]
fn encode_uri_component_matches_javascript() {
    assert_eq!(encode_uri_component("a b.txt"), "a%20b.txt");
    assert_eq!(encode_uri_component("x_y-z!~*'()"), "x_y-z!~*'()");
    assert_eq!(encode_uri_component("é"), "%C3%A9");
}

#[test]
fn format_bytes_units() {
    assert_eq!(format_bytes(0), "0B");
    assert_eq!(format_bytes(1023), "1023B");
    assert_eq!(format_bytes(1536), "1.5KB");
    assert_eq!(format_bytes(2_000_000), "1.9MB");
}
