//! Small, portable image previews. Decode on a worker; painting touches only bounded pixel data.
use super::{
    Rows, text,
    theme::{Theme, Tok},
};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use image::{ImageReader, RgbaImage, imageops::FilterType};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use serde_json::Value;
use std::{io::Cursor, sync::Arc};

pub const MAX_COLUMNS: u32 = 80;
pub const MAX_ROWS: u32 = 16;
const MAX_PIXELS: u64 = 16 * 1024 * 1024;

pub struct Thumbnail {
    label: String,
    original: (u32, u32),
    pixels: RgbaImage,
    native_png: Option<Arc<str>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageProtocol {
    Kitty,
    Iterm,
}

impl ImageProtocol {
    pub fn detect() -> Option<Self> {
        let env = |key| std::env::var(key).unwrap_or_default();
        Self::from_terminal(
            &env("TERM"),
            &env("TERM_PROGRAM"),
            !env("TMUX").is_empty() || !env("STY").is_empty(),
        )
    }

    fn from_terminal(term: &str, program: &str, multiplexer: bool) -> Option<Self> {
        if multiplexer {
            return None;
        }
        if matches!(term, "xterm-kitty" | "xterm-ghostty") || matches!(program, "ghostty" | "kitty")
        {
            Some(Self::Kitty)
        } else if program == "iTerm.app" {
            Some(Self::Iterm)
        } else {
            None
        }
    }
}

/// Trusted graphics payload, generated only from validated and bounded decoded image bytes.
#[derive(Clone)]
pub struct NativeImage {
    png: Arc<str>,
    columns: u32,
    rows: u32,
    indent: u32,
}

impl NativeImage {
    pub fn rows(&self) -> usize {
        self.rows as usize
    }

    pub(crate) fn fits(&self, columns: u16, rows: u16) -> bool {
        self.columns + self.indent <= u32::from(columns) && self.rows < u32::from(rows)
    }

    pub(crate) fn write_ansi(&self, out: &mut String, protocol: ImageProtocol) {
        // The driver has reserved all image rows plus the separator. Draw upward into that space,
        // leaving room for terminals that advance the cursor below the image, then restore it.
        // Ordinary tool output can never enter this trusted protocol path.
        out.push_str("\x1b7");
        out.push_str(&format!("\x1b[{}A", self.rows));
        out.push_str(&format!("\x1b[{}G", self.indent + 1));
        match protocol {
            ImageProtocol::Iterm => out.push_str(&format!(
                "\x1b]1337;File=inline=1;width={};height={};preserveAspectRatio=1:{}\x07",
                self.columns, self.rows, self.png
            )),
            ImageProtocol::Kitty => {
                let chunks = self.png.as_bytes().chunks(4096);
                let count = chunks.len();
                for (index, chunk) in chunks.enumerate() {
                    let more = u8::from(index + 1 < count);
                    if index == 0 {
                        out.push_str(&format!(
                            "\x1b_Ga=T,f=100,t=d,q=2,C=1,c={},r={},m={more};",
                            self.columns, self.rows
                        ));
                    } else {
                        out.push_str(&format!("\x1b_Gm={more};"));
                    }
                    out.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
                    out.push_str("\x1b\\");
                }
            }
        }
        out.push_str("\x1b8");
    }
}

impl Thumbnail {
    /// The caller runs this off the terminal input thread. References never grant new read access.
    pub fn load(reference: &Value) -> Result<Self> {
        Self::load_with_native(reference, false)
    }

    pub fn load_with_native(reference: &Value, native: bool) -> Result<Self> {
        let (bytes, _) = crate::llm::images::verified_bytes(reference)?;
        let reader = || ImageReader::new(Cursor::new(&bytes)).with_guessed_format();
        let (width, height) = reader()?.into_dimensions()?;
        ensure!(
            width > 0 && height > 0 && u64::from(width) * u64::from(height) <= MAX_PIXELS,
            "terminal preview exceeds the 16-megapixel decode budget"
        );
        let mut decoder = reader()?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(16_384);
        limits.max_image_height = Some(16_384);
        limits.max_alloc = Some(64 * 1024 * 1024);
        decoder.limits(limits);
        let decoded = decoder.decode().context("decoding terminal preview")?;
        let native_png = if native {
            let mut encoded = Cursor::new(Vec::new());
            decoded
                .resize(1280.min(width), 1280.min(height), FilterType::Lanczos3)
                .write_to(&mut encoded, image::ImageFormat::Png)?;
            Some(Arc::from(STANDARD.encode(encoded.into_inner())))
        } else {
            None
        };
        let pixels = decoded
            .thumbnail(MAX_COLUMNS.min(width), (MAX_ROWS * 2).min(height))
            .to_rgba8();
        Ok(Self {
            label: reference["path"].as_str().unwrap_or("image").to_owned(),
            original: (width, height),
            pixels,
            native_png,
        })
    }

    pub fn native(&self, width: u16) -> Option<NativeImage> {
        let png = self.native_png.clone()?;
        if width == 0 {
            return None;
        }
        let indent = if width > 4 { 2 } else { 0 };
        let room = u32::from(width)
            .saturating_sub(indent)
            .min(MAX_COLUMNS)
            .max(1);
        let columns = self.pixels.width().min(room);
        let height = ((u64::from(self.pixels.height()) * u64::from(columns))
            / u64::from(self.pixels.width()))
        .max(1) as u32;
        Some(NativeImage {
            png,
            columns,
            rows: height.div_ceil(2),
            indent,
        })
    }

    pub fn rows(&self, width: u16, theme: &Theme) -> Rows {
        if width == 0 {
            return Vec::new();
        }
        let indent = if width > 4 { 2 } else { 0 };
        let room = u32::from(width)
            .saturating_sub(indent)
            .min(MAX_COLUMNS)
            .max(1);
        let pixels = if self.pixels.width() > room {
            let height = ((u64::from(self.pixels.height()) * u64::from(room))
                / u64::from(self.pixels.width()))
            .max(1) as u32;
            image::imageops::resize(&self.pixels, room, height, FilterType::Triangle)
        } else {
            self.pixels.clone()
        };
        let separator = if theme.ascii() { " | " } else { " · " };
        let mut rows = vec![Line::from(Span::styled(
            text::clip(
                &text::ellipsize(
                    &text::one_line(&format!(
                        "Image{separator}{}{separator}{}x{}",
                        self.label, self.original.0, self.original.1,
                    )),
                    width as usize,
                    theme.ascii(),
                ),
                width as usize,
            ),
            theme.fg(Tok::Muted),
        ))];
        for y in (0..pixels.height()).step_by(2) {
            let mut spans = vec![Span::raw(" ".repeat(indent as usize))];
            for x in 0..pixels.width() {
                let top = composite(pixels.get_pixel(x, y).0);
                let bottom = composite(pixels.get_pixel(x, (y + 1).min(pixels.height() - 1)).0);
                if theme.ascii() {
                    spans.push(Span::styled(
                        " ",
                        Style::default().bg(Color::Rgb(
                            ((u16::from(top.0) + u16::from(bottom.0)) / 2) as u8,
                            ((u16::from(top.1) + u16::from(bottom.1)) / 2) as u8,
                            ((u16::from(top.2) + u16::from(bottom.2)) / 2) as u8,
                        )),
                    ));
                } else {
                    spans.push(Span::styled(
                        "▀",
                        Style::default()
                            .fg(Color::Rgb(top.0, top.1, top.2))
                            .bg(Color::Rgb(bottom.0, bottom.1, bottom.2)),
                    ));
                }
            }
            rows.push(Line::from(spans));
        }
        rows
    }
}

// A neutral matte makes transparent pixels visible without guessing the terminal's background.
fn composite(pixel: [u8; 4]) -> (u8, u8, u8) {
    let channel = |value| {
        ((u32::from(value) * u32::from(pixel[3]) + 32 * (255 - u32::from(pixel[3]))) / 255) as u8
    };
    (channel(pixel[0]), channel(pixel[1]), channel(pixel[2]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::Glyphs;

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
            };
            let (rows, images) = transcript.take_settled_with_images(90, &ctx);
            assert_eq!(images.len(), 1);
            assert!(transcript.take_settled_with_images(90, &ctx).1.is_empty());
            let output = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let mut terminal =
                crate::ui::term::with_writer(Box::new(Output(output.clone())), (90, 30));
            terminal.commit_with_images(rows, images, protocol);
            let view = [Line::raw("composer")];
            let viewport = crate::ui::term::Viewport {
                rows: &view,
                cursor: Some((0, 8)),
                title: None,
            };
            terminal.paint(&viewport).unwrap();
            terminal.paint(&viewport).unwrap();
            let bytes = output.lock().unwrap();
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
            crate::llm::images::attachment(root.path(), std::path::Path::new("viewed.png"))
                .unwrap();
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
            crate::llm::images::attachment(root.path(), std::path::Path::new("viewed.png"))
                .unwrap();
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
}
