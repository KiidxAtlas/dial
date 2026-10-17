use super::*;
#[test]
fn retained_pixels_survive_source_overwrite_deletion_and_storage_reopen() {
    let root = tempfile::tempdir().unwrap();
    let blobs = tempfile::tempdir().unwrap();
    let path = root.path().join("shot.png");
    fs::write(&path, tiny_png()).unwrap();
    let mut message = json!({"role":"user", "content":"inspect", "dial_images":[attachment(root.path(), Path::new("shot.png")).unwrap()]});
    crate::storage::media::retain_message(
        &mut message,
        &crate::tools::artifact::ArtifactStore::new(blobs.path()).unwrap(),
    );
    let durable = serde_json::to_vec(&message).unwrap();
    assert!(
        durable.len() < 1024,
        "only compact references enter the journal"
    );
    fs::write(&path, [tiny_png(), b"new screenshot".to_vec()].concat()).unwrap();
    assert_eq!(
        verified_bytes(&message["dial_images"][0]).unwrap().0,
        tiny_png()
    );
    fs::remove_file(&path).unwrap();
    let reopened: Value = serde_json::from_slice(&durable).unwrap();
    assert_eq!(
        verified_bytes(&reopened["dial_images"][0]).unwrap().0,
        tiny_png()
    );
    let snapshot = &reopened["dial_images"][0]["snapshot"];
    fs::write(
        Path::new(snapshot["root"].as_str().unwrap()).join(snapshot["path"].as_str().unwrap()),
        b"tampered",
    )
    .unwrap();
    assert!(verified_bytes(&reopened["dial_images"][0]).is_err());
}

#[test]
fn capture_failure_never_substitutes_replacement_pixels() {
    let root = tempfile::tempdir().unwrap();
    let blobs = tempfile::tempdir().unwrap();
    let path = root.path().join("shot.png");
    fs::write(&path, tiny_png()).unwrap();
    let mut message =
        json!({"dial_images":[attachment(root.path(), Path::new("shot.png")).unwrap()]});
    fs::remove_file(&path).unwrap();
    crate::storage::media::retain_message(
        &mut message,
        &crate::tools::artifact::ArtifactStore::new(blobs.path()).unwrap(),
    );
    fs::write(&path, tiny_png()).unwrap();
    assert!(
        verified_bytes(&message["dial_images"][0])
            .unwrap_err()
            .to_string()
            .contains("capture failed")
    );
}
#[test]
fn stale_image_notices_do_not_repeat_or_mutate_request_history() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("shot.png");
    fs::write(&path, tiny_png()).unwrap();
    let reference = attachment(root.path(), Path::new("shot.png")).unwrap();
    let req = super::super::Chat {
        messages: vec![json!({"role":"user","content":"inspect","dial_images":[reference]})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 100,
        seed: None,
    };
    let original = req.messages.clone();
    let tracker = NoticeTracker::default();
    let mut notices = Vec::new();
    fs::write(&path, [tiny_png(), b"changed".to_vec()].concat()).unwrap();
    let mut previous = None;
    for _ in 0..5 {
        let prepared = prepare(&req, true, &tracker, &mut |n| notices.push(n.to_owned())).unwrap();
        let content = prepared.messages[0]["content"].as_str().unwrap();
        assert_eq!(
            content,
            "inspect\n\n[Image unavailable: shot.png; changed; reattach]"
        );
        if let Some(previous) = &previous {
            assert_eq!(&prepared.messages, previous);
        }
        previous = Some(prepared.messages);
    }
    assert_eq!(notices.len(), 1);
    assert_eq!(req.messages, original);

    // A fresh reference to the replacement gets its pixels; the old reference remains invalid.
    let mut refreshed = req.clone();
    refreshed.messages[0]["dial_images"] =
        json!([attachment(root.path(), Path::new("shot.png")).unwrap()]);
    let prepared = prepare(&refreshed, true, &tracker, &mut |_| panic!("fresh image")).unwrap();
    assert_eq!(prepared.messages[0]["content"][1]["type"], "image_url");
    // Successful recovery re-arms the warning if the newly attached file changes again.
    fs::write(&path, tiny_png()).unwrap();
    prepare(&refreshed, true, &tracker, &mut |n| {
        notices.push(n.to_owned())
    });
    assert_eq!(notices.len(), 2);
    let recovered = prepare(&req, true, &tracker, &mut |_| panic!("restored image")).unwrap();
    assert_eq!(recovered.messages[0]["content"][1]["type"], "image_url");
}

#[test]
fn image_notices_distinguish_failure_changes_and_model_capabilities() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("shot.png");
    fs::write(&path, tiny_png()).unwrap();
    let reference = attachment(root.path(), Path::new("shot.png")).unwrap();
    let req = super::super::Chat {
        messages: vec![
            json!({"role":"user","content":"inspect","dial_images":[reference.clone(), reference]}),
        ],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 100,
        seed: None,
    };
    let tracker = NoticeTracker::default();
    let mut notices = Vec::new();
    for _ in 0..2 {
        prepare(&req, false, &tracker, &mut |n| notices.push(n.to_owned()));
    }
    assert_eq!(
        notices.len(),
        1,
        "duplicate references and requests share a notice"
    );
    prepare(&req, true, &tracker, &mut |_| panic!("vision works"));
    fs::remove_file(path).unwrap();
    for _ in 0..2 {
        prepare(&req, true, &tracker, &mut |n| notices.push(n.to_owned()));
    }
    assert_eq!(notices.len(), 2);
    assert!(notices[1].contains("file is unavailable"));
    assert_eq!(
        tracker.0.lock().len(),
        1,
        "only current failures are retained"
    );
}

#[test]
fn browser_screenshot_pixels_survive_artifact_storage_and_provider_encoding() {
    let root = tempfile::tempdir().unwrap();
    let store = crate::tools::artifact::ArtifactStore::new(root.path()).unwrap();
    let encoded = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aY4cAAAAASUVORK5CYII=";
    let envelope =
        json!({"artifact":{"media_type":"image/png","encoding":"base64","data":encoded}});
    let mut output = crate::tools::ToolOutput {
        execution: Default::default(),
        data: Some(envelope.clone()),
        text: envelope.to_string(),
        is_error: false,
    };
    let image = crate::tools::artifact::capture_image(Some(&store), &mut output)
        .unwrap()
        .unwrap();
    let req = super::super::Chat {
        messages: vec![
            json!({"role":"user","content":"inspect the rendered page"}),
            json!({"role":"assistant","content":null,"tool_calls":[{"id":"capture","type":"function","function":{"name":"browser","arguments":"{\"action\":\"screenshot\"}"}}]}),
            json!({"role":"tool","tool_call_id":"capture","content":output.text,"dial_images":[image]}),
        ],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 100,
        seed: None,
    };
    assert!(!req.messages.iter().any(|m| m.to_string().contains(encoded)));
    let prepared = prepare(&req, true, &NoticeTracker::default(), &mut |_| {
        panic!("valid screenshot should attach")
    })
    .unwrap();
    assert_eq!(prepared.messages[2]["role"], "tool");
    assert_eq!(
        prepared.messages[3]["role"], "user",
        "pixels follow the complete tool results"
    );
    let pixels = &prepared.messages[3]["content"][0];
    assert_eq!(
        pixels["image_url"]["url"],
        format!("data:image/png;base64,{encoded}")
    );
    let anthropic = anthropic_content(&prepared.messages[3]["content"]);
    assert_eq!(anthropic[0]["source"]["data"], encoded);
    let responses = super::super::responses::request("vision", &prepared, None).unwrap();
    assert_eq!(responses["input"][3]["content"][0]["type"], "input_image");
    assert_eq!(
        responses["input"][3]["content"][0]["image_url"],
        pixels["image_url"]["url"]
    );
    assert!(!responses.to_string().contains("dial_images"));
}
#[test]
fn unavailable_images_are_explained_to_the_model_as_well_as_the_user() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("tiny.png"), b"\x89PNG\r\n\x1a\noriginal").unwrap();
    let reference = attachment(root.path(), Path::new("tiny.png")).unwrap();
    let req = super::super::Chat {
        messages: vec![json!({"role":"user","content":"inspect it","dial_images":[reference]})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 100,
        seed: None,
    };
    for vision in [false, true] {
        if vision {
            fs::remove_file(root.path().join("tiny.png")).unwrap();
        }
        let mut notices = Vec::new();
        let prepared = prepare(&req, vision, &NoticeTracker::default(), &mut |notice| {
            notices.push(notice.to_string())
        })
        .unwrap();
        assert_eq!(notices.len(), 1);
        assert!(
            prepared.messages[0]["content"]
                .as_str()
                .unwrap()
                .contains("Image unavailable")
        );
        assert!(
            !prepared
                .messages
                .iter()
                .any(|m| m.to_string().contains("image_url"))
        );
    }
}
#[test]
fn compact_references_reject_changed_oversized_and_escaped_images() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("tiny.png"), tiny_png()).unwrap();
    let reference = attachment(root.path(), Path::new("tiny.png")).unwrap();
    assert!(reference.get("data").is_none());
    let part = image_part(&reference).unwrap();
    assert!(
        part["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,")
    );
    fs::write(
        root.path().join("tiny.png"),
        [tiny_png(), b"edit".to_vec()].concat(),
    )
    .unwrap();
    assert!(image_part(&reference).is_err());
    fs::remove_file(root.path().join("tiny.png")).unwrap();
    assert!(image_part(&reference).is_err());
    assert!(attachment(root.path(), Path::new("../escape.png")).is_err());
    let outside = tempfile::tempdir().unwrap();
    fs::write(
        outside.path().join("image.png"),
        b"\x89PNG\r\n\x1a\noutside",
    )
    .unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(
            outside.path().join("image.png"),
            root.path().join("link.png"),
        )
        .unwrap();
        assert!(attachment(root.path(), Path::new("link.png")).is_err());
    }
    let large = fs::File::create(root.path().join("large.png")).unwrap();
    large.set_len(MAX_IMAGE_BYTES + 1).unwrap();
    assert!(attachment(root.path(), Path::new("large.png")).is_err());
}

/// A real 1x1 PNG; decoding validates the header chain before any pixels reach a provider.
fn tiny_png() -> Vec<u8> {
    STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aY4cAAAAASUVORK5CYII=")
        .unwrap()
}
