use super::*;
use crate::ui::theme::Glyphs;

#[test]
fn dedicated_viewer_images_use_available_width_but_fit_the_viewport() {
    let (_root, reference) = fixture(1600, 900, image::ImageFormat::Png);
    let thumbnail = Thumbnail::load_with_native(&reference, true).unwrap();
    let large = thumbnail.native_fitted(160, 60).unwrap();
    assert_eq!(large.columns, 160);
    assert_eq!(large.rows, 45);
    assert!(thumbnail.native(160).unwrap().rows <= MAX_ROWS);
    let short = thumbnail.native_fitted(160, 10).unwrap();
    assert!(short.columns <= 160 && short.rows <= 10);
    assert!(thumbnail.native_fitted(0, 10).is_none());
    assert!(thumbnail.native_fitted(160, 0).is_none());
}
#[test]
fn native_protocols_are_selected_only_for_supported_direct_terminals() {
    for (term, program, protocol) in [
        ("xterm-kitty", "", Some(ImageProtocol::Kitty)),
        ("xterm-ghostty", "ghostty", Some(ImageProtocol::Kitty)),
        ("xterm-256color", "iTerm.app", Some(ImageProtocol::Iterm)),
        ("xterm-256color", "Apple_Terminal", None),
        ("dumb", "", None),
    ] {
        assert_eq!(ImageProtocol::from_terminal(term, program, false), protocol);
        assert_eq!(ImageProtocol::from_terminal(term, program, true), None);
    }
}

#[test]
fn native_preview_preserves_detail_and_commits_once_without_displacing_composer() {
    struct Output(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let (_root, reference) = fixture(1600, 900, image::ImageFormat::Png);
    let thumbnail = Thumbnail::load_with_native(&reference, true).unwrap();
    let png = STANDARD
        .decode(thumbnail.native_png.as_ref().unwrap().as_bytes())
        .unwrap();
    assert_eq!(
        image::load_from_memory(&png)
            .unwrap()
            .into_rgba8()
            .dimensions(),
        (1280, 720)
    );
    assert!(thumbnail.pixels.height() <= MAX_ROWS * 2);
    for protocol in [ImageProtocol::Kitty, ImageProtocol::Iterm] {
        let mut transcript = crate::ui::transcript::Transcript::new();
        transcript.image(Thumbnail::load_with_native(&reference, true).unwrap());
        let theme = Theme::dark();
        let ui = crate::llm::model_config::UiConfig::default();
        let ctx = crate::ui::transcript::TranscriptCtx {
            theme: &theme,
            ui: &ui,
            tick: 0,
            tools_expanded: false,
            expand_key: "Ctrl+O",
        };
        let (rows, images) = transcript.take_settled_with_images(90, &ctx);
        assert_eq!(images.len(), 1);
        assert!(transcript.take_settled_with_images(90, &ctx).1.is_empty());
        let output = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut terminal = crate::ui::term::with_writer(Box::new(Output(output.clone())), (90, 30));
        terminal.commit_with_images(rows, images, protocol);
        let view = [Line::raw("composer")];
        let viewport = crate::ui::term::Viewport {
            rows: &view,
            cursor: Some((0, 8)),
            title: None,
            signals: None,
        };
        terminal.paint(&viewport).unwrap();
        terminal.paint(&viewport).unwrap();
        let bytes = output.lock().unwrap().clone();
        let ansi = std::str::from_utf8(&bytes).unwrap();
        let marker = if protocol == ImageProtocol::Kitty {
            "a=T,f=100"
        } else {
            "]1337;File="
        };
        assert_eq!(ansi.matches(marker).count(), 1);
        let mut screen = vt100::Parser::new(30, 90, 100);
        screen.process(&bytes);
        assert!(screen.screen().contents().contains("composer"));
        assert_eq!(screen.screen().cursor_position().1, 8);
    }
}

fn fixture(width: u32, height: u32, format: image::ImageFormat) -> (tempfile::TempDir, Value) {
    let root = tempfile::tempdir().unwrap();
    let pixels = RgbaImage::from_fn(width, height, |_, y| {
        if y < height / 2 {
            image::Rgba([255, 0, 0, 255])
        } else {
            image::Rgba([0, 0, 255, 255])
        }
    });
    let image = image::DynamicImage::ImageRgba8(pixels);
    let image = if format == image::ImageFormat::Jpeg {
        image::DynamicImage::ImageRgb8(image.to_rgb8())
    } else {
        image
    };
    let mut bytes = Cursor::new(Vec::new());
    image.write_to(&mut bytes, format).unwrap();
    std::fs::write(root.path().join("viewed.png"), bytes.into_inner()).unwrap();
    let reference =
        crate::llm::images::attachment(root.path(), std::path::Path::new("viewed.png")).unwrap();
    (root, reference)
}

#[test]
fn previews_decode_supported_formats_and_preserve_real_pixel_colors() {
    for format in [
        image::ImageFormat::Png,
        image::ImageFormat::Jpeg,
        image::ImageFormat::Gif,
        image::ImageFormat::WebP,
    ] {
        let (_root, reference) = fixture(2, 2, format);
        let thumbnail = Thumbnail::load(&reference).unwrap();
        assert_eq!(
            thumbnail.pixels.dimensions(),
            (2, 2),
            "small images are not enlarged"
        );
        let rows = thumbnail.rows(80, &Theme::dark());
        assert_eq!(rows.len(), 2);
        if format != image::ImageFormat::Jpeg {
            assert_eq!(rows[1].spans[1].style.fg, Some(Color::Rgb(255, 0, 0)));
            assert_eq!(rows[1].spans[1].style.bg, Some(Color::Rgb(0, 0, 255)));
        }
    }
}

#[test]
fn previews_fit_narrow_terminals_and_bound_large_and_portrait_images() {
    for (width, height) in [(1600, 900), (900, 1600), (1200, 1200)] {
        let (_root, reference) = fixture(width, height, image::ImageFormat::Png);
        let thumbnail = Thumbnail::load(&reference).unwrap();
        assert!(thumbnail.pixels.width() <= MAX_COLUMNS);
        assert!(thumbnail.pixels.height() <= MAX_ROWS * 2);
        let ratio = thumbnail.pixels.width() as f64 / thumbnail.pixels.height() as f64;
        assert!((ratio - width as f64 / height as f64).abs() < 0.1);
        for glyphs in [Glyphs::Unicode, Glyphs::Nerd, Glyphs::Ascii] {
            let theme = Theme::dark().with_glyphs(glyphs);
            for columns in [0, 1, 2, 4, 10, 30, 80, 200] {
                let rows = thumbnail.rows(columns, &theme);
                assert!(rows.len() <= MAX_ROWS as usize + 1);
                assert!(rows.iter().all(|row| row.width() <= columns as usize));
                if glyphs == Glyphs::Ascii {
                    assert!(rows.iter().skip(1).all(|row| row.to_string().is_ascii()));
                }
            }
        }
    }
}

#[test]
fn preview_rejects_changed_and_escaped_references_and_handles_transparency() {
    let (root, reference) = fixture(2, 2, image::ImageFormat::Png);
    std::fs::write(root.path().join("viewed.png"), b"changed").unwrap();
    assert!(Thumbnail::load(&reference).is_err());
    let mut escaped = reference;
    escaped["path"] = serde_json::json!("../outside.png");
    assert!(Thumbnail::load(&escaped).is_err());
    assert_eq!(composite([255, 0, 0, 0]), (32, 32, 32));
    assert_eq!(composite([255, 0, 0, 255]), (255, 0, 0));
}

#[test]
fn compressed_images_cannot_bypass_the_decoded_pixel_budget() {
    let (root, _) = fixture(2, 2, image::ImageFormat::Png);
    let path = root.path().join("viewed.png");
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[16..20].copy_from_slice(&5_000u32.to_be_bytes());
    bytes[20..24].copy_from_slice(&5_000u32.to_be_bytes());
    let mut crc = !0u32;
    for &byte in &bytes[12..29] {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320u32 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    bytes[29..33].copy_from_slice(&(!crc).to_be_bytes());
    std::fs::write(&path, bytes).unwrap();
    let reference =
        crate::llm::images::attachment(root.path(), std::path::Path::new("viewed.png")).unwrap();
    let error = Thumbnail::load(&reference).err().unwrap().to_string();
    assert!(error.contains("16-megapixel"), "{error}");
}

#[test]
fn image_blocks_settle_once_and_are_written_as_colored_terminal_cells() {
    use std::{
        io::Write,
        sync::{Arc, Mutex},
    };
    #[derive(Clone, Default)]
    struct Output(Arc<Mutex<Vec<u8>>>);
    impl Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let (_root, reference) = fixture(2, 2, image::ImageFormat::Png);
    let mut transcript = crate::ui::transcript::Transcript::new();
    transcript.image(Thumbnail::load(&reference).unwrap());
    let theme = Theme::dark();
    let ui = crate::llm::model_config::UiConfig::default();
    let ctx = crate::ui::transcript::TranscriptCtx {
        theme: &theme,
        ui: &ui,
        tick: 0,
        tools_expanded: false,
        expand_key: "Ctrl+O",
    };
    let settled = transcript.take_settled(80, &ctx);
    assert!(settled[0].to_string().contains("viewed.png"));
    assert!(transcript.take_settled(80, &ctx).is_empty());
    let output = Output::default();
    let mut terminal = crate::ui::term::with_writer(Box::new(output.clone()), (80, 24));
    terminal.commit(settled);
    terminal
        .paint(&crate::ui::term::Viewport {
            rows: &[Line::raw("composer")],
            cursor: None,
            title: None,
            signals: None,
        })
        .unwrap();
    let mut screen = vt100::Parser::new(24, 80, 100);
    screen.process(&output.0.lock().unwrap());
    let pixel = screen.screen().cell(1, 2).unwrap();
    assert_eq!(pixel.contents(), "▀");
    assert_eq!(pixel.fgcolor(), vt100::Color::Rgb(255, 0, 0));
    assert_eq!(pixel.bgcolor(), vt100::Color::Rgb(0, 0, 255));
    assert!(screen.screen().contents().contains("composer"));
}

#[test]
fn placements_draw_at_their_cell_for_each_protocol() {
    let placed = [Placement {
        x: 3,
        y: 4,
        image: NativeImage {
            png: Arc::from("AAAA"),
            columns: 2,
            rows: 1,
            indent: 0,
        },
    }];
    let mut out = String::new();
    write_placements(&mut out, ImageProtocol::Iterm, &placed);
    assert!(out.contains("\x1b[5;4H"));
    assert!(out.contains("1337;File=inline=1;width=2;height=1"));
    let mut out = String::new();
    write_placements(&mut out, ImageProtocol::Kitty, &placed);
    assert!(out.starts_with("\x1b_Ga=d,d=A,q=2\x1b\\"));
    assert!(out.contains("\x1b[5;4H"));
    assert!(out.contains("a=T,f=100"));
}
