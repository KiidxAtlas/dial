use super::super::{Format, Source};
use super::*;
use base64::engine::general_purpose::STANDARD;

fn req(size: &'static str, output: &'static str) -> Request<'static> {
    Request {
        prompt: "a red fox",
        size,
        output,
        source: None,
    }
}

fn encoded(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

#[test]
fn model_id_strips_one_models_prefix_and_rejects_unsafe_ids() {
    assert_eq!(
        model_id("models/gemini-2.5-flash-image").unwrap(),
        "gemini-2.5-flash-image"
    );
    assert_eq!(
        model_id("imagen-4.0-generate-001").unwrap(),
        "imagen-4.0-generate-001"
    );
    assert!(model_id("models/models/x").is_err());
    assert!(model_id("").is_err());
    assert!(model_id("models/").is_err());
    assert!(model_id("a/b").is_err());
    assert!(model_id("a:b").is_err());
    assert!(model_id("has space").is_err());
    assert!(model_id(&"a".repeat(MAX_MODEL_CHARS + 1)).is_err());
}

#[test]
fn gemini_body_requests_text_and_image_with_aspect_ratio() {
    let body = gemini_body(&req("1536x1024", "out.png"));
    assert_eq!(
        body["generationConfig"]["responseModalities"],
        json!(["TEXT", "IMAGE"])
    );
    assert_eq!(
        body["generationConfig"]["imageConfig"]["aspectRatio"],
        "3:2"
    );
    assert_eq!(body["contents"][0]["parts"][0]["text"], "a red fox");
    assert_eq!(body["contents"][0]["parts"].as_array().unwrap().len(), 1);
}

#[test]
fn gemini_body_omits_aspect_ratio_for_auto() {
    let body = gemini_body(&req("auto", "out.png"));
    assert!(body["generationConfig"].get("imageConfig").is_none());
}

#[test]
fn gemini_body_sends_source_as_inline_data() {
    let mut req = req("1024x1024", "out.png");
    req.source = Some(Source {
        relative: "in.png".into(),
        bytes: vec![1, 2, 3],
        format: Format::Png,
    });
    let body = gemini_body(&req);
    let inline = &body["contents"][0]["parts"][1]["inlineData"];
    assert_eq!(inline["mimeType"], "image/png");
    assert_eq!(inline["data"], encoded(&[1, 2, 3]));
}

#[test]
fn gemini_image_returns_the_only_image_and_skips_thoughts() {
    let response = json!({"candidates": [{"finishReason": "STOP", "content": {"parts": [
        {"thought": true, "inlineData": {"mimeType": "image/png", "data": encoded(b"draft")}},
        {"text": "here it is"},
        {"inlineData": {"mimeType": "image/png", "data": encoded(b"final")}}
    ]}}]});
    assert_eq!(gemini_image(&response).unwrap(), b"final");
}

#[test]
fn gemini_image_reports_refusals_and_missing_images() {
    let finish =
        |reason: &str| json!({"candidates": [{"finishReason": reason, "content": {"parts": []}}]});
    let safety = gemini_image(&finish("SAFETY")).unwrap_err().to_string();
    assert!(safety.contains("SAFETY"), "{safety}");
    let no_image = gemini_image(&finish("NO_IMAGE")).unwrap_err().to_string();
    assert!(no_image.contains("NO_IMAGE"), "{no_image}");
    let stopped = gemini_image(&finish("MAX_TOKENS")).unwrap_err().to_string();
    assert!(stopped.contains("MAX_TOKENS"), "{stopped}");
    let text_only =
        json!({"candidates": [{"finishReason": "STOP", "content": {"parts": [{"text": "no"}]}}]});
    assert!(gemini_image(&text_only).is_err());
    let blocked = json!({"promptFeedback": {"blockReason": "PROHIBITED_CONTENT"}});
    assert!(
        gemini_image(&blocked)
            .unwrap_err()
            .to_string()
            .contains("PROHIBITED_CONTENT")
    );
    assert!(gemini_image(&json!({})).is_err());
}

#[test]
fn gemini_image_rejects_multiple_images() {
    let response = json!({"candidates": [{"finishReason": "STOP", "content": {"parts": [
        {"inlineData": {"mimeType": "image/png", "data": encoded(b"a")}},
        {"inlineData": {"mimeType": "image/png", "data": encoded(b"b")}}
    ]}}]});
    let error = gemini_image(&response).unwrap_err().to_string();
    assert!(error.contains("2 images"), "{error}");
}

#[test]
fn imagen_rejects_the_gemini_developer_api_with_a_migration_message() {
    let error = check_imagen_base("https://generativelanguage.googleapis.com/v1beta")
        .unwrap_err()
        .to_string();
    assert!(error.contains("shut down"), "{error}");
    assert!(error.contains("migrate"), "{error}");
    assert!(check_imagen_base(
        "https://us-central1-aiplatform.googleapis.com/v1/projects/p/locations/us-central1/publishers/google"
    )
    .is_ok());
    assert!(check_imagen_base("not a url").is_err());
}

#[test]
fn imagen_body_uses_one_sample_output_mime_and_aspect_ratio() {
    let body = imagen_body(&req("1536x1024", "out.JPG")).unwrap();
    assert_eq!(body["instances"][0]["prompt"], "a red fox");
    assert_eq!(body["parameters"]["sampleCount"], 1);
    assert_eq!(
        body["parameters"]["outputOptions"]["mimeType"],
        "image/jpeg"
    );
    assert_eq!(body["parameters"]["aspectRatio"], "4:3");
    let auto = imagen_body(&req("auto", "out.png")).unwrap();
    assert_eq!(auto["parameters"]["outputOptions"]["mimeType"], "image/png");
    assert!(auto["parameters"].get("aspectRatio").is_none());
}

#[test]
fn imagen_body_rejects_unsupported_output_extension() {
    assert!(imagen_body(&req("1024x1024", "out.gif")).is_err());
    assert!(imagen_body(&req("1024x1024", "out.webp")).is_err());
}

#[test]
fn imagen_rejects_source_images_before_any_request() {
    let mut request = req("1024x1024", "out.png");
    assert!(ensure_no_edit(&request).is_ok());
    request.source = Some(Source {
        relative: "in.png".into(),
        bytes: vec![1],
        format: Format::Png,
    });
    let error = ensure_no_edit(&request).unwrap_err().to_string();
    assert!(error.contains("not supported"), "{error}");
}

#[test]
fn imagen_image_returns_the_single_prediction() {
    let response =
        json!({"predictions": [{"bytesBase64Encoded": encoded(b"png"), "mimeType": "image/png"}]});
    assert_eq!(imagen_image(&response).unwrap(), b"png");
}

#[test]
fn imagen_image_reports_empty_filtered_and_multiple_predictions() {
    assert!(imagen_image(&json!({"predictions": []})).is_err());
    assert!(imagen_image(&json!({})).is_err());
    let filtered = json!({"predictions": [{"raiFilteredReason": "PROHIBITED"}]});
    let error = imagen_image(&filtered).unwrap_err().to_string();
    assert!(error.contains("PROHIBITED"), "{error}");
    let many = json!({"predictions": [
        {"bytesBase64Encoded": encoded(b"a")},
        {"bytesBase64Encoded": encoded(b"b")}
    ]});
    assert!(
        imagen_image(&many)
            .unwrap_err()
            .to_string()
            .contains("2 images")
    );
}

#[test]
fn decode_bounded_rejects_invalid_base64() {
    assert_eq!(decode_bounded(&encoded(b"abc")).unwrap(), b"abc");
    assert!(decode_bounded("!!not base64!!").is_err());
}
