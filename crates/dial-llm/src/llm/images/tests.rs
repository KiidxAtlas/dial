use super::*;
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
fn unavailable_images_are_explained_to_the_model_as_well_as_the_user() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("tiny.png"), tiny_png()).unwrap();
    let reference = attachment(root.path(), Path::new("tiny.png")).unwrap();
    let req = super::super::Chat {
        messages: vec![json!({"role":"user","content":"inspect it","dial_images":[reference]})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 100,
        seed: None,
    };
    let mut notices = Vec::new();
    let text_only = prepare(&req, false, &NoticeTracker::default(), &mut |notice| {
        notices.push(notice.to_string())
    })
    .unwrap();
    assert_eq!(notices.len(), 1);
    assert!(
        !text_only
            .messages
            .iter()
            .any(|m| m.to_string().contains("image_url"))
    );

    fs::remove_file(root.path().join("tiny.png")).unwrap();
    let mut notices = Vec::new();
    let prepared = prepare(&req, true, &NoticeTracker::default(), &mut |notice| {
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
    large.set_len(MAX_IMAGE_SOURCE_BYTES + 1).unwrap();
    assert!(attachment(root.path(), Path::new("large.png")).is_err());
}

#[test]
fn oversized_images_are_downscaled_under_the_model_cap() {
    let (width, height) = (2000u32, 2000u32);
    let mut state: u32 = 0x1234_5678;
    let pixels: Vec<u8> = (0..width * height * 3)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        })
        .collect();
    let mut png = Vec::new();
    image::RgbImage::from_raw(width, height, pixels)
        .unwrap()
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    assert!(png.len() as u64 > MAX_IMAGE_BYTES);
    let (bytes, mime) = resize_for_model(&png, "image/png").unwrap();
    assert!(bytes.len() as u64 <= MAX_IMAGE_BYTES);
    // Incompressible pixels cannot fit the cap as PNG; the encoder falls back to JPEG.
    assert!(
        matches!(mime.as_str(), "image/png" | "image/jpeg"),
        "{mime}"
    );
    let decoded = image::load_from_memory(&bytes).unwrap();
    assert!(decoded.width() <= width && decoded.width() > 0);
}

#[test]
fn question_reads_split_only_on_a_non_empty_path_and_question() {
    assert_eq!(
        split_question("shot.png?q=what does this show?"),
        Some(("shot.png", "what does this show?"))
    );
    assert_eq!(split_question("shot.png"), None);
    assert_eq!(split_question("shot.png?q=   "), None);
    assert_eq!(split_question("?q=what"), None);
}

/// A real 1x1 PNG; decoding validates the header chain before any pixels reach a provider.
fn tiny_png() -> Vec<u8> {
    STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aY4cAAAAASUVORK5CYII=")
        .unwrap()
}
