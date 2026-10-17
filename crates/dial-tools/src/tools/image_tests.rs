//! Image attachments that cross storage (retained pixels, artifact capture) and the provider wire.
use crate::llm::{
    Chat,
    images::{NoticeTracker, anthropic_content, attachment, prepare, verified_bytes},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{fs, path::Path};

#[test]
fn retained_pixels_survive_source_overwrite_deletion_and_storage_reopen() {
    let root = tempfile::tempdir().unwrap();
    let blobs = tempfile::tempdir().unwrap();
    let path = root.path().join("shot.png");
    fs::write(&path, tiny_png()).unwrap();
    let mut message = json!({"role":"user", "content":"inspect", "dial_images":[attachment(root.path(), Path::new("shot.png")).unwrap()]});
    crate::storage::media::retain_message(
        &mut message,
        &crate::storage::artifact::ArtifactStore::new(blobs.path()).unwrap(),
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
        &crate::storage::artifact::ArtifactStore::new(blobs.path()).unwrap(),
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
fn browser_screenshot_pixels_survive_artifact_storage_and_provider_encoding() {
    let root = tempfile::tempdir().unwrap();
    let store = crate::storage::artifact::ArtifactStore::new(root.path()).unwrap();
    let encoded = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aY4cAAAAASUVORK5CYII=";
    let envelope =
        json!({"artifact":{"media_type":"image/png","encoding":"base64","data":encoded}});
    let mut output = crate::tools::ToolOutput {
        execution: Default::default(),
        data: Some(envelope.clone()),
        text: envelope.to_string(),
        is_error: false,
    };
    let image = crate::storage::artifact::capture_image(Some(&store), &mut output)
        .unwrap()
        .unwrap();
    let req = Chat {
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
    let responses = crate::llm::responses::request("vision", &prepared, None).unwrap();
    assert_eq!(responses["input"][3]["content"][0]["type"], "input_image");
    assert_eq!(
        responses["input"][3]["content"][0]["image_url"],
        pixels["image_url"]["url"]
    );
    assert!(!responses.to_string().contains("dial_images"));
}

fn tiny_png() -> Vec<u8> {
    STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aY4cAAAAASUVORK5CYII=")
        .unwrap()
}
