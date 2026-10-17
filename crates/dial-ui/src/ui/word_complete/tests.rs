use super::*;
use std::sync::atomic::AtomicU64;

fn learned(prompts: &[&str]) -> WordModel {
    let mut model = WordModel::default();
    for prompt in prompts {
        model.learn_prompt(prompt);
    }
    model
}

#[test]
fn frequent_recent_words_outrank_rare_ones() {
    let model = learned(&["dataset dataset", "database", "database", "database"]);
    assert_eq!(model.complete("da"), Some("tabase"));
}

#[test]
fn stale_frequent_words_fade_behind_recent_ones() {
    let mut prompts: Vec<String> = (0..5).map(|_| "amazing".to_string()).collect();
    for _ in 0..300 {
        prompts.push("unrelated filler".to_string());
    }
    prompts.push("ambient".to_string());
    let refs: Vec<&str> = prompts.iter().map(String::as_str).collect();
    let model = learned(&refs);
    assert_eq!(model.complete("am"), Some("bient"));
}

#[test]
fn suffix_keeps_learned_casing_and_ignores_typed_case() {
    let model = learned(&["Database"]);
    assert_eq!(model.complete("dat"), Some("abase"));
    assert_eq!(model.complete("DAT"), Some("abase"));
}

#[test]
fn exact_words_and_short_prefixes_do_not_complete() {
    let model = learned(&["database"]);
    assert_eq!(model.complete("database"), None);
    assert_eq!(model.complete("d"), None);
    assert_eq!(model.complete("zzz"), None);
}

#[test]
fn paths_mentions_and_urls_are_never_learned() {
    assert_eq!(
        learnable_words("open src/parser/mod.rs and @file_name now https://example.com/page"),
        vec!["open", "and", "now"]
    );
}

#[test]
fn secret_looking_tokens_are_not_learned() {
    assert!(!is_learnable_word("abc123def456ghi"));
    assert!(!is_learnable_word("deadbeefcafebabe0"));
    assert!(!is_learnable_word("12345"));
    assert!(!is_learnable_word("ab"));
    assert!(is_learnable_word("compute_total"));
}

#[test]
fn prompt_with_a_detected_secret_teaches_nothing() {
    let mut model = WordModel::default();
    let secret = format!("sk-{}", "a".repeat(48));
    let prompt = format!("please rotate {secret} soon");
    assert!(redact_text_counted(&prompt).1 > 0);
    assert!(!model.learn_prompt(&prompt));
    assert!(model.is_empty());
    assert!(model.learn_prompt("rotate tokens soon"));
    assert_eq!(model.complete("rot"), Some("ate"));
}

#[test]
fn learned_model_is_bounded_and_keeps_recent_words() {
    let mut model = WordModel::default();
    for n in 0..(MAX_LEARNED_WORDS + 500) {
        model.learn_prompt(&format!("word{n:05}x"));
    }
    assert!(model.len() <= MAX_LEARNED_WORDS);
    assert!(model.len() > EVICT_TO);
    let newest = format!("word{:05}x", MAX_LEARNED_WORDS + 499);
    assert!(model.index.words.contains_key(&newest));
}

#[test]
fn persistence_round_trip_keeps_completions_and_ticks() {
    let model = learned(&["database", "database", "deploy now"]);
    let bytes = model.to_bytes().unwrap();
    let back = WordModel::from_bytes(&bytes).unwrap();
    assert_eq!(back.len(), model.len());
    assert_eq!(back.complete("dat"), model.complete("dat"));
    assert_eq!(back.tick, model.tick);
}

#[test]
fn corrupt_or_foreign_files_are_errors_not_silent_resets() {
    assert!(WordModel::from_bytes(b"not json").is_err());
    assert!(WordModel::from_bytes(br#"{"version":9,"tick":0,"words":[]}"#).is_err());
}

#[test]
fn stored_words_are_revalidated_on_load() {
    let bytes = br#"{"version":1,"tick":3,"words":[{"word":"abc123def456ghi","count":4,"last":3},{"word":"valid","count":0,"last":99}]}"#;
    let model = WordModel::from_bytes(bytes).unwrap();
    assert_eq!(model.len(), 1);
    assert_eq!(model.index.words["valid"].count, 1);
    assert_eq!(model.index.words["valid"].last, 3);
}

fn scratch_dir(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("dial-word-{name}-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn vocabulary_uses_identifiers_and_file_stems_but_skips_binary_files() {
    let root = scratch_dir("vocab");
    std::fs::write(
        root.join("main.rs"),
        "fn compute_total() { compute_total(); }",
    )
    .unwrap();
    std::fs::write(root.join("blob.bin"), b"compute_secret\0\0\0 more").unwrap();
    std::fs::write(root.join("report_builder.md"), "no code here").unwrap();
    let files = vec![
        "main.rs".to_string(),
        "blob.bin".to_string(),
        "report_builder.md".to_string(),
    ];
    let current = AtomicU64::new(1);
    let index = build_vocabulary(&root, &files, &current, 1);
    assert_eq!(completion_from_project(&index, "comp"), Some("ute_total"));
    assert_eq!(completion_from_project(&index, "compute_s"), None);
    assert_eq!(completion_from_project(&index, "rep"), Some("ort_builder"));
    assert_eq!(completion_from_project(&index, "builder"), None);
    assert_eq!(completion_from_project(&index, "buil"), None);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn superseded_scan_returns_nothing() {
    let root = scratch_dir("cancel");
    std::fs::write(root.join("a.rs"), "alpha_beta").unwrap();
    let current = AtomicU64::new(2);
    let index = build_vocabulary(&root, &["a.rs".to_string()], &current, 1);
    assert!(index.is_empty());
    let _ = std::fs::remove_dir_all(&root);
}
