use super::*;

fn layout(root: &Path) -> Layout {
    let cwd = root.join("cwd");
    fs::create_dir_all(&cwd).unwrap();
    Layout::with_user_dir(&cwd, root.join("home")).unwrap()
}

#[test]
fn traversal_in_parent_id_stays_inside_subagents() {
    let root = tempfile::tempdir().unwrap();
    let layout = layout(root.path());
    let dir = transcript_dir(&layout, "../../etc");
    assert_eq!(dir.parent().unwrap(), layout.state_dir.join("subagents"));
    assert_eq!(safe_component(""), "session");
}

#[test]
fn lists_only_regular_jsonl_files_and_skips_symlinks() {
    let root = tempfile::tempdir().unwrap();
    let layout = layout(root.path());
    let dir = transcript_dir(&layout, "parent");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("task-1-1.jsonl"), "{}\n").unwrap();
    fs::write(dir.join("notes.txt"), "x").unwrap();
    let outside = root.path().join("secret.jsonl");
    fs::write(&outside, "{}\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, dir.join("linked.jsonl")).unwrap();

    let files = transcript_files(&layout, "parent").unwrap();
    let names: Vec<_> = files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["task-1-1.jsonl".to_string()]);
}

#[test]
fn missing_directory_is_no_workers() {
    let root = tempfile::tempdir().unwrap();
    let layout = layout(root.path());
    assert!(workers(&layout, "nobody").unwrap().is_empty());
}

#[test]
fn reads_header_title_and_message_records_only() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("job-1.jsonl");
    fs::write(
        &path,
        concat!(
            "{\"type\":\"header\",\"id\":\"task-7\"}\n",
            "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
            "not json\n",
            "{\"type\":\"status\",\"state\":\"done\"}\n",
        ),
    )
    .unwrap();
    let worker = read_worker(&path).unwrap();
    assert_eq!(worker.title, "task-7");
    assert_eq!(worker.messages.len(), 1);
    assert_eq!(worker.messages[0]["content"], "hi");
}
