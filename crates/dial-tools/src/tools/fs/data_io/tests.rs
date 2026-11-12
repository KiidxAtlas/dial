use super::*;
use crate::tools::testkit;

#[test]
fn automatic_input_capture_preserves_small_sources_and_excludes_credentials() {
    let (dir, ws) = testkit::workspace();
    let private = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(
        crate::storage::artifact::ArtifactStore::new(&private.path().join("artifacts")).unwrap(),
    );
    let checkpoints = crate::storage::checkpoint::Checkpoints::new(
        private.path().join("checkpoints"),
        dir.path().to_owned(),
    )
    .unwrap();
    let turn = checkpoints
        .begin_conversation_turn("recover", "test")
        .unwrap();
    for name in [
        "main.db",
        "main.db-wal",
        ".env",
        "credentials.json",
        "private.key",
    ] {
        std::fs::write(dir.path().join(name), [0, 255, 42]).unwrap();
    }
    let result =
        preserve_inputs_with_budget(&ws, &checkpoints, turn, std::time::Duration::from_secs(5));
    let files = result["files"].as_array().unwrap();
    assert_eq!(files.len(), 2, "{result}");
    assert!(
        files
            .iter()
            .all(|file| file["path"].as_str().unwrap().starts_with("main.db"))
    );
    std::fs::remove_file(dir.path().join("main.db-wal")).unwrap();
    let original = files
        .iter()
        .find(|file| file["path"] == "main.db-wal")
        .unwrap();
    let retained = read(&ws, &json!({"path":original["artifact"],"format":"base64"})).unwrap();
    assert_eq!(retained.data.unwrap()["content"], "AP8q");
    assert!(checkpoints.changed_files(turn).is_empty());
}

#[test]
fn exact_reads_keep_binary_and_crlf_bytes_out_of_the_summary() {
    let (dir, ws) = testkit::workspace();
    let bytes = b"a\r\n\0\xff";
    std::fs::write(dir.path().join("input.dat"), bytes).unwrap();
    let output = read(&ws, &json!({"path":"input.dat","format":"base64"})).unwrap();
    let data = output.data.unwrap();
    assert_eq!(
        STANDARD.decode(data["content"].as_str().unwrap()).unwrap(),
        bytes
    );
    assert_eq!(data["bytes"], bytes.len());
    assert!(
        !output.text.contains("content"),
        "data must stay outside the summary"
    );
    assert!(read(&ws, &json!({"path":"input.dat","format":"text"})).is_err());
    std::fs::write(dir.path().join("text.txt"), "a\r\nb\n").unwrap();
    assert_eq!(
        read(&ws, &json!({"path":"text.txt","format":"text"}))
            .unwrap()
            .data
            .unwrap()["content"],
        "a\r\nb\n"
    );
}

#[test]
fn data_reads_enforce_size_scope_and_representation() {
    let (dir, ws) = testkit::workspace();
    std::fs::write(dir.path().join("big"), vec![0; MAX_BYTES + 1]).unwrap();
    for args in [
        json!({"path":"big","format":"base64"}),
        json!({"path":"../outside","format":"text"}),
        json!({"path":"big","format":"base64","offset":1}),
        json!({"path":"big","format":"text","offset":1,"retain":true}),
        json!({"path":"big","format":"text","offset":0}),
        json!({"path":"../outside","format":"text","offset":1}),
        json!({"path":"big","format":"json"}),
    ] {
        assert!(read(&ws, &args).is_err(), "{args}");
    }
    let whole = read(&ws, &json!({"path":"big","format":"text"})).unwrap_err();
    assert!(whole.contains("offset/limit"), "{whole}");
}

#[test]
fn line_pages_cover_files_beyond_the_exact_read_limit() {
    let (dir, ws) = testkit::workspace();
    let line = format!("{}\r\n", "x".repeat(1023));
    let lines = 1500;
    std::fs::write(dir.path().join("big.jsonl"), line.repeat(lines)).unwrap();
    let page = |offset: u64, limit: Option<u64>| {
        let mut args = json!({"path":"big.jsonl","format":"text","offset":offset});
        if let Some(limit) = limit {
            args["limit"] = json!(limit);
        }
        read(&ws, &args).unwrap().data.unwrap()
    };
    let (mut offset, mut seen, mut joined) = (1, 0, String::new());
    loop {
        let data = page(offset, None);
        let content = data["content"].as_str().unwrap();
        assert!(content.len() <= MAX_BYTES);
        seen += data["lines"].as_u64().unwrap();
        joined.push_str(content);
        match data["next_offset"].as_u64() {
            Some(next) => offset = next,
            None => break,
        }
    }
    assert_eq!(seen, lines as u64);
    assert_eq!(joined, line.repeat(lines));
    let two = page(1499, Some(5));
    assert_eq!(
        (two["lines"].as_u64(), two["next_offset"].is_null()),
        (Some(2), true)
    );
    let one = page(2, Some(1));
    assert_eq!(
        (one["content"].as_str(), one["next_offset"].as_u64()),
        (Some(line.as_str()), Some(3))
    );
    let past = page(5000, None);
    assert_eq!(
        (past["lines"].as_u64(), past["content"].as_str()),
        (Some(0), Some(""))
    );
}

#[test]
fn binary_publication_is_lossless_exclusive_and_cancelled_before_commit() {
    let (dir, ws) = testkit::workspace();
    let config = crate::config::types::Config::default();
    let authorize = testkit::authorize_in_workspace(&ws, "write");
    let ctx = ToolContext {
        workspace: &ws,
        config: &config,
        mutation_authorizer: Some(&authorize),
        confirm: None,
    };
    let args =
        json!({"path":"output.bin","content":STANDARD.encode([0,255,13,10]),"encoding":"base64"});
    assert!(write(&ctx, &args, &AtomicBool::new(true)).is_err());
    assert!(!dir.path().join("output.bin").exists());
    let output = write(&ctx, &args, &AtomicBool::new(false)).unwrap();
    assert_eq!(output.execution.changed_paths.len(), 1);
    assert_eq!(
        std::fs::read(dir.path().join("output.bin")).unwrap(),
        [0, 255, 13, 10]
    );
    assert!(
        write(&ctx, &args, &AtomicBool::new(false)).is_err(),
        "existing bytes must survive"
    );
    assert_eq!(
        std::fs::read(dir.path().join("output.bin")).unwrap(),
        [0, 255, 13, 10]
    );
    let denied = ToolContext {
        mutation_authorizer: None,
        ..ctx
    };
    assert!(
        write(
            &denied,
            &json!({"path":"denied.bin","content":"AA=="}),
            &AtomicBool::new(false)
        )
        .is_err()
    );
    assert!(!dir.path().join("denied.bin").exists());
}

#[test]
fn retained_sources_survive_removal_of_the_original() {
    let (dir, ws) = testkit::workspace();
    let ws = ws.with_artifacts(
        crate::storage::artifact::ArtifactStore::new(&dir.path().join("artifacts")).unwrap(),
    );
    std::fs::write(dir.path().join("damaged-wal"), [0, 255, 42]).unwrap();
    let output = read(
        &ws,
        &json!({"path":"damaged-wal","format":"base64","retain":true}),
    )
    .unwrap();
    let reference = output.data.unwrap()["artifact"]
        .as_str()
        .unwrap()
        .to_owned();
    std::fs::remove_file(dir.path().join("damaged-wal")).unwrap();
    assert!(reference.starts_with("artifact://"), "{reference}");
    // The normal artifact byte reader remains the authority for reopening retained bytes.
    let result = crate::tools::read_tool_streaming(
        &ws,
        &json!({"path":format!("{reference}:bytes=0+3")}),
        &AtomicBool::new(false),
        4096,
    );
    assert!(!result.is_error, "{}", result.text);
    assert!(result.text.contains("AP8q"), "{}", result.text);
}

#[test]
fn artifact_byte_reads_return_whole_inputs_and_page_losslessly_within_the_budget() {
    let (dir, ws) = testkit::workspace();
    let ws = ws.with_artifacts(
        crate::storage::artifact::ArtifactStore::new(&dir.path().join("artifacts")).unwrap(),
    );
    // The size of a real retained WAL that a fixed 16 KiB page could not return in one read.
    let bytes: Vec<u8> = (0..16_512u32).map(|n| (n * 7 % 256) as u8).collect();
    let uri = ws.artifacts().unwrap().retain_exact_bytes(&bytes).unwrap();
    let page = |selector: String, budget| {
        let result = crate::tools::read_tool_streaming(
            &ws,
            &json!({ "path": selector }),
            &AtomicBool::new(false),
            budget,
        );
        assert!(!result.is_error, "{}", result.text);
        assert!(result.text.len() <= budget, "{}", result.text.len());
        let value: Value = serde_json::from_str(&result.text).unwrap();
        let data = STANDARD.decode(value["data"].as_str().unwrap()).unwrap();
        (data, value["next_offset"].as_u64().unwrap())
    };
    let (whole, next) = page(format!("{uri}:bytes=0"), 48 * 1024);
    assert_eq!((whole.as_slice(), next), (bytes.as_slice(), 16_512));
    let mut rebuilt = Vec::new();
    while (rebuilt.len() as u64) < 16_512 {
        let (data, next) = page(format!("{uri}:bytes={}+20000", rebuilt.len()), 4096);
        assert!(!data.is_empty() && data.len() < 4096);
        rebuilt.extend(data);
        assert_eq!(next, rebuilt.len() as u64);
    }
    assert_eq!(rebuilt, bytes);
}
