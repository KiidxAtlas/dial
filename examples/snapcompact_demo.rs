//! Demo: generate snapcompact PNG frames for visual inspection.
//! Run with: cargo run --example snapcompact_demo

use serde_json::json;
use std::sync::atomic::AtomicBool;

fn main() -> anyhow::Result<()> {
    let messages = vec![
        json!({
            "role": "user",
            "content": "Hello! Can you help me understand how snapcompact works?"
        }),
        json!({
            "role": "assistant",
            "content": "Snapcompact is a conversation-to-PNG rasterizer. It takes LLM conversation messages and renders them as pixel-perfect bitmap images using embedded fonts like 5x8, 8x8, 6x12, 8x13, and Silver TrueType. Each frame is a PNG that can be fed back into an LLM as visual context."
        }),
        json!({
            "role": "user",
            "content": "That's interesting! What shapes are available?"
        }),
        json!({
            "role": "assistant",
            "content": "There are 17 shape variants: 8x8r-bw, 8x8r-sent, 8x8u-bw, 8x8u-sent, 6x6u-bw, 6x6u-sent, 5x8-bw, 5x8-sent, 6x12-dim, 8x13-bw, 8on16-bw, 8on22-bw, 11on16-bw, silver16-bw, doc-8on16-bw, doc-8on16-sent, and doc-8on16-sent-dim. The 'bw' shapes use black-and-white ink, while 'sent' shapes color-code sentences with six hues (red, amber, green, teal, blue, violet). The 'doc' shapes render in two columns."
        }),
    ];

    let cancel = AtomicBool::new(false);

    // Shape 1: 8x13-bw (classic black & white)
    let config_bw = dial_llm::config::snapcompact::SnapcompactConfig {
        shape: "8x13-bw".to_string(),
        ..dial_llm::config::snapcompact::SnapcompactConfig::default()
    };
    let archive_bw = dial_agent::agent::compact::snapcompact::archive_with_options(
        &messages, &config_bw, 64, &cancel,
    )?;
    save_first_png(&archive_bw, "/tmp/snapcompact_8x13bw.png")?;
    println!(
        "Saved 8x13-bw: {}x{} pixels",
        archive_bw.frames[0].width, archive_bw.frames[0].height
    );

    // Shape 2: 8x8r-sent (sentence-hued, 8x8 bitmap)
    let config_sent = dial_llm::config::snapcompact::SnapcompactConfig {
        shape: "8x8r-sent".to_string(),
        ..dial_llm::config::snapcompact::SnapcompactConfig::default()
    };
    let archive_sent = dial_agent::agent::compact::snapcompact::archive_with_options(
        &messages,
        &config_sent,
        64,
        &cancel,
    )?;
    save_first_png(&archive_sent, "/tmp/snapcompact_8x8r_sent.png")?;
    println!(
        "Saved 8x8r-sent: {}x{} pixels",
        archive_sent.frames[0].width, archive_sent.frames[0].height
    );

    // Shape 3: doc-8on16-sent (two-column doc layout)
    let config_doc = dial_llm::config::snapcompact::SnapcompactConfig {
        shape: "doc-8on16-sent".to_string(),
        ..dial_llm::config::snapcompact::SnapcompactConfig::default()
    };
    let archive_doc = dial_agent::agent::compact::snapcompact::archive_with_options(
        &messages,
        &config_doc,
        64,
        &cancel,
    )?;
    save_first_png(&archive_doc, "/tmp/snapcompact_doc_8on16_sent.png")?;
    println!(
        "Saved doc-8on16-sent: {}x{} pixels",
        archive_doc.frames[0].width, archive_doc.frames[0].height
    );

    // Shape 4: 5x8-bw (tiny bitmap)
    let config_5x8 = dial_llm::config::snapcompact::SnapcompactConfig {
        shape: "5x8-bw".to_string(),
        ..dial_llm::config::snapcompact::SnapcompactConfig::default()
    };
    let archive_5x8 = dial_agent::agent::compact::snapcompact::archive_with_options(
        &messages,
        &config_5x8,
        64,
        &cancel,
    )?;
    save_first_png(&archive_5x8, "/tmp/snapcompact_5x8bw.png")?;
    println!(
        "Saved 5x8-bw: {}x{} pixels",
        archive_5x8.frames[0].width, archive_5x8.frames[0].height
    );

    println!("\nAll PNGs saved to /tmp/");
    Ok(())
}

fn save_first_png(
    archive: &dial_agent::agent::compact::snapcompact::Archive,
    path: &str,
) -> anyhow::Result<()> {
    let content = &archive.messages[0];
    let content_arr = content["content"].as_array().unwrap();

    for item in content_arr {
        if let Some(img) = item.get("image_url")
            && let Some(url) = img["url"].as_str()
            && let Some(base64_data) = url.strip_prefix("data:image/png;base64,")
        {
            let png_bytes =
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, base64_data)?;
            std::fs::write(path, &png_bytes)?;
        }
    }
    Ok(())
}
