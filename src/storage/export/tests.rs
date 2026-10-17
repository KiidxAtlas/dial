use super::*;
use serde_json::json;

#[test]
fn export_redacts_messages_tool_calls_and_results() {
    let key = format!("sk-ant-{}", "Q".repeat(40));
    let user = json!({"role":"user","content":format!("use {key}")});
    let call = json!({"role":"assistant","content":"","tool_calls":[{"id":"c1","type":"function","function":{"name":"bash","arguments":format!("{{\"command\":\"curl -H 'Authorization: Bearer {key}'\"}}")}}]});
    let result = json!({"role":"tool","tool_call_id":"c1","content":format!("echo {key}")});
    for html in [false, true] {
        let text = render(&format!("title {key}"), &[&user, &call, &result], html);
        assert!(!text.contains(&key), "{text}");
        assert!(text.contains("[REDACTED:"), "{text}");
    }
}

#[test]
fn markdown_uses_fences_not_html() {
    let result = json!({"role":"tool","tool_call_id":"c1","content":"```\n<b>x</b>"});
    let text = render("<t>", &[&result], false);
    assert!(text.starts_with("# <t>"));
    assert!(text.contains("````\n```\n<b>x</b>\n````"));
    assert!(!text.contains("<details>"));
}

#[test]
fn html_embeds_verified_images_and_reports_missing_ones() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let path = root.join("pixel.png");
    let mut png = Vec::new();
    image::DynamicImage::new_rgb8(1, 1)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    std::fs::write(&path, &png).unwrap();
    let reference = crate::llm::images::user_attachment(&root, &path).unwrap();
    let message = json!({"role":"user","content":"look","dial_images":[reference, {"root":"/nonexistent","path":"x.png","sha256":"00"}]});
    let text = render("t", &[&message], true);
    assert!(
        text.contains("<img alt=\"attached image\" src=\"data:image/png;base64,"),
        "{text}"
    );
    assert!(text.contains("Stored image unavailable"), "{text}");
}
