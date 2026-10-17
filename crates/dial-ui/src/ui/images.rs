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

    /// Columns the image is indented by inside the width it was built for.
    pub(crate) fn indent(&self) -> u16 {
        self.indent as u16
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
        self.write_payload(out, protocol);
        out.push_str("\x1b8");
    }

    /// Draws the image with its top-left cell at zero-based screen column `x`, row `y`, then restores the cursor.
    pub(crate) fn write_at(&self, out: &mut String, protocol: ImageProtocol, x: u16, y: u16) {
        out.push_str("\x1b7");
        out.push_str(&format!("\x1b[{};{}H", u32::from(y) + 1, u32::from(x) + 1));
        self.write_payload(out, protocol);
        out.push_str("\x1b8");
    }

    fn write_payload(&self, out: &mut String, protocol: ImageProtocol) {
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
    }
}

/// A native image whose top-left cell sits at zero-based screen column `x`, row `y`.
#[derive(Clone)]
pub struct Placement {
    pub x: u16,
    pub y: u16,
    pub image: NativeImage,
}

/// Draws placements after a frame. Kitty images are deleted first so a redraw never stacks copies.
pub(crate) fn write_placements(
    out: &mut String,
    protocol: ImageProtocol,
    placements: &[Placement],
) {
    if protocol == ImageProtocol::Kitty {
        out.push_str("\x1b_Ga=d,d=A,q=2\x1b\\");
    }
    for placed in placements {
        placed.image.write_at(out, protocol, placed.x, placed.y);
    }
}

impl Thumbnail {
    /// The caller runs this off the terminal input thread. References never grant new read access.
    #[cfg(test)]
    pub fn load(reference: &Value) -> Result<Self> {
        Self::load_with_native(reference, false)
    }

    pub fn load_with_native(reference: &Value, native: bool) -> Result<Self> {
        let (bytes, _) = crate::llm::images::verified_bytes(reference)?;
        let label = reference["path"].as_str().unwrap_or("image");
        Self::from_bytes(label, &bytes, native)
    }

    /// Decodes bounded raster bytes the caller already read. Never reads files itself.
    pub fn from_bytes(label: &str, bytes: &[u8], native: bool) -> Result<Self> {
        let reader = || ImageReader::new(Cursor::new(bytes)).with_guessed_format();
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
            label: label.to_owned(),
            original: (width, height),
            pixels,
            native_png,
        })
    }

    pub fn dimensions(&self) -> (u32, u32) {
        self.original
    }

    pub fn native(&self, width: u16) -> Option<NativeImage> {
        let png = self.native_png.clone()?;
        if width == 0 {
            return None;
        }
        let indent = if width > 4 { 2 } else { 0 };
        let room = u32::from(width)
            .saturating_sub(indent)
            .clamp(1, MAX_COLUMNS);
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
            .clamp(1, MAX_COLUMNS);
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
mod tests;
