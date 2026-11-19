use super::*;

#[test]
fn request_asks_for_visual_and_code_review_and_names_changed_files() {
    let text = request(&["src/app.css".into(), "src/app.rs".into()]);
    assert!(text.starts_with("[Dial quality review]"));
    assert!(text.contains("fresh screenshot"));
    assert!(text.contains("run the relevant tests, linters or type"));
    assert!(text.ends_with("Changed files: src/app.css, src/app.rs"));
}

#[test]
fn request_bounds_the_listed_paths() {
    let paths: Vec<String> = (0..15).map(|i| format!("f{i}.rs")).collect();
    let text = request(&paths);
    assert!(text.contains("f11.rs (+3 more)"));
    assert!(!text.contains("f12.rs"));
    assert!(!request(&[]).contains("Changed files"));
}
