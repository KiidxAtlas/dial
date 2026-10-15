//! Native snapcompact rasterizer. Contains third-party MIT-licensed code; see THIRD_PARTY_NOTICES.
//! Copyright (c) 2025 Mario Zechner, 2025-2026 Can Bölük, 2026 Stencil Labs, Inc.
//! Distributed under MIT; full permission notice: THIRD_PARTY_NOTICES.
//! Silver font: Poppy Works / Wolfgang Wozniak, CC BY 4.0; see fonts/Silver.LICENSE.

use std::{
    borrow::Cow, collections::HashMap, f32::consts::PI, iter::Peekable, str::Chars, sync::LazyLock,
};

use anyhow::{Result, bail};
use fontdue::{Font as TtfFace, FontSettings, Metrics};
use std::sync::atomic::{AtomicBool, Ordering};

/// Upper bound on the frame edge: a hard stop against absurd allocations
/// (`size * size` pixel buffer), far above the 2576px production frame.
const MAX_FRAME_SIZE: u32 = 2576;

/// Indexed palette: 0 is the white background, 1-6 are the six dark sentence
/// hues from the eval renderer (HLS l=0.22 s=0.95, h ∈ {0, .08, .3, .5, .62,
/// .78}), 7 is plain black ink (`bw` variant), 8 is the pale highlight band
/// behind repeated line copies, 9 is the dim gray ink for tool-output spans.
const PALETTE: [[u8; 3]; 10] = [
    [255, 255, 255],
    [109, 2, 2],     // red
    [109, 53, 2],    // amber
    [24, 109, 2],    // green
    [2, 109, 109],   // teal
    [2, 32, 109],    // blue
    [75, 2, 109],    // violet
    [0, 0, 0],       // bw ink
    [255, 247, 194], // repeat highlight band
    [128, 128, 128], // dim ink (tool-output spans)
];
const INK_COLORS: usize = 6;
const INK_BLACK: u8 = 7;
const BG_REPEAT: u8 = 8;
const INK_DIM: u8 = 9;
/// Zero-width ink toggles embedded in the text stream (shift-out/shift-in).
const DIM_ON: u32 = 0x0e;
const DIM_OFF: u32 = 0x0f;
/// FULL BLOCK: fills its entire cell box with pitch-black ink (`INK_BLACK`,
/// ignoring sentence hue and dim state). The TypeScript normalizer folds
/// newline runs to it.
const FULL_BLOCK: u32 = 0x2588;

static FONT_5X8: LazyLock<Font> = LazyLock::new(|| parse_bdf(include_str!("fonts/5x8.bdf"), 5, 8));
static FONT_8X8: LazyLock<Font> = LazyLock::new(|| parse_hex(include_str!("fonts/unscii-8.hex")));
static FONT_6X12: LazyLock<Font> =
    LazyLock::new(|| parse_bdf(include_str!("fonts/6x12.bdf"), 6, 12));
static FONT_8X13: LazyLock<Font> =
    LazyLock::new(|| parse_bdf(include_str!("fonts/8x13.bdf"), 8, 13));
/// Silver's design size and natural cell. Kept as constants so bitmap shapes
/// can size their fallback glyphs without forcing the multi-megabyte TTF parse
/// (`FONT_SILVER` is only touched when a code point misses the bitmap font).
const SILVER_PX: f32 = 16.0;
const SILVER_CELL: usize = 16;
static FONT_SILVER: LazyLock<TtfFont> = LazyLock::new(|| {
    let face = TtfFace::from_bytes(
        include_bytes!("fonts/Silver.ttf").as_slice(),
        FontSettings::default(),
    )
    .expect("bundled Silver.ttf must parse");
    let ascent = face
        .horizontal_line_metrics(SILVER_PX)
        .map_or(SILVER_PX * 0.8, |metrics| metrics.ascent);
    TtfFont { face, ascent }
});

struct Glyph {
    /// Glyph width in pixels (≤ 8 for the bundled fonts).
    w: u8,
    /// Glyph height in pixels.
    h: i32,
    xoff: i32,
    yoff: i32,
    /// One bitmask per bitmap row, MSB-leftmost.
    rows: Vec<u8>,
}

struct Font {
    /// Glyphs keyed by Unicode code point (ASCII + Latin-1 coverage).
    glyphs: HashMap<u32, Glyph>,
    ascent: i32,
    /// Natural cell advance (x) in pixels.
    cell_w: usize,
    /// Natural cell pitch (y) in pixels.
    cell_h: usize,
}

struct TtfFont {
    face: TtfFace,
    /// Ascent at `SILVER_PX`, the fallback when a size has no line metrics.
    ascent: f32,
}

struct RasterizedGlyph {
    metrics: Metrics,
    bitmap: Vec<u8>,
}

fn parse_bdf(text: &str, cell_w: usize, cell_h: usize) -> Font {
    let mut glyphs = HashMap::new();
    let mut ascent = 0i32;
    let mut enc = -1i64;
    let mut bbx = [0i32; 4];
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        if let Some(rest) = line.strip_prefix("FONT_ASCENT") {
            ascent = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("ENCODING") {
            enc = rest.trim().parse().unwrap_or(-1);
        } else if let Some(rest) = line.strip_prefix("BBX") {
            let mut parts = rest.split_ascii_whitespace();
            for slot in &mut bbx {
                *slot = parts.next().and_then(|part| part.parse().ok()).unwrap_or(0);
            }
        } else if line.starts_with("BITMAP") {
            let mut rows = Vec::new();
            for row in lines.by_ref() {
                if row.starts_with("ENDCHAR") {
                    break;
                }
                rows.push(u8::from_str_radix(row.trim(), 16).unwrap_or(0));
            }
            if enc >= 0 {
                glyphs.insert(
                    enc as u32,
                    Glyph {
                        w: bbx[0].clamp(0, 8) as u8,
                        h: bbx[1],
                        xoff: bbx[2],
                        yoff: bbx[3],
                        rows,
                    },
                );
            }
        }
    }
    Font {
        glyphs,
        ascent,
        cell_w,
        cell_h,
    }
}

/// Parse a unifont-style `.hex` font (`CODEPOINT:16-hex-digit bitmap`, one
/// byte per row of an 8x8 glyph). Baseline sits at row 7 (`ascent` 7 with a
/// one-pixel descender row), matching the eval renderer.
fn parse_hex(text: &str) -> Font {
    let mut glyphs = HashMap::new();
    for line in text.lines() {
        let Some((cp, bits)) = line.split_once(':') else {
            continue;
        };
        let Ok(enc) = u32::from_str_radix(cp.trim(), 16) else {
            continue;
        };
        let bits = bits.trim();
        if bits.len() != 16 {
            continue;
        }
        let rows: Vec<u8> = (0..8)
            .map(|i| u8::from_str_radix(&bits[i * 2..i * 2 + 2], 16).unwrap_or(0))
            .collect();
        glyphs.insert(
            enc,
            Glyph {
                w: 8,
                h: 8,
                xoff: 0,
                yoff: -1,
                rows,
            },
        );
    }
    Font {
        glyphs,
        ascent: 7,
        cell_w: 8,
        cell_h: 8,
    }
}

enum RenderFont<'a> {
    Bitmap(&'a Font),
    Ttf(&'a TtfFont),
}

fn resolve_font(name: &str) -> Option<RenderFont<'static>> {
    match name {
        "5x8" => Some(RenderFont::Bitmap(&FONT_5X8)),
        "8x8" => Some(RenderFont::Bitmap(&FONT_8X8)),
        "6x12" => Some(RenderFont::Bitmap(&FONT_6X12)),
        "8x13" => Some(RenderFont::Bitmap(&FONT_8X13)),
        "silver" => Some(RenderFont::Ttf(&FONT_SILVER)),
        _ => None,
    }
}

/// Frame grid geometry shared with the TypeScript caller. The cell box
/// (`cell_w` x `cell_h`) is the advance/pitch glyphs are laid out on; it may
/// differ from the font's natural cell (e.g. 8x13 glyphs on an 8x16 pitch).
struct Grid {
    cols: usize,
    rows: usize,
    repeat: usize,
    /// Cell advance (x) in pixels.
    cell_w: usize,
    /// Cell pitch (y) in pixels.
    cell_h: usize,
}

/// East Asian Wide / Fullwidth code points that occupy two grid cells when
/// drawn through the Silver fallback in a narrow bitmap shape. The same ranges
/// are mirrored in `packages/snapcompact/src/snapcompact.ts` so the TypeScript
/// capacity/pagination math and this layout never disagree on cell counts.
const fn is_wide(cp: u32) -> bool {
    matches!(cp,
        0x1100..=0x115F
        | 0x2E80..=0x2EFF
        | 0x2F00..=0x2FDF
        | 0x3000..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x20000..=0x2FFFD
        | 0x30000..=0x3FFFD
    )
}

/// One inked cell yielded by [`Layout`]: the character, its resolved palette
/// ink, the pixel x origin of its (first) cell, the grid row band it prints
/// on, and how many cells it spans.
struct LaidCell {
    ch: char,
    ink: u8,
    x: usize,
    row: usize,
    units: usize,
}

/// The single layout/ink state machine behind every renderer and the canvas
/// height (`used_rows`), so cell accounting and drawing cannot drift apart.
///
/// Grid layout is row-major with no word wrap; characters beyond
/// `cols * rows` are dropped. Doc layout (`columns: 2`) splits on `'\n'`
/// (zero-width): line `li` lands at column `li / rows`, row `li % rows`; each
/// column is `(cols - GUTTER) / 2` cells wide, the second starts
/// `col_w + GUTTER` cells in; overlong lines clip and lines past the second
/// column are dropped (the TypeScript caller pre-wraps and paginates).
///
/// `U+000E`/`U+000F` toggle dim ink without occupying a cell. Ink is dim, else
/// black for `bw`, else one of six hues that advances after a terminator in
/// `.!?` followed by a space or full block (doc mode: also a newline). With
/// `wide_cells` (bitmap shapes) East Asian wide code points take two cells and
/// are padded to the next row/column edge rather than split across it.
///
/// The walk ends at the first `None` (capacity reached, past the second doc
/// column, or end of text); it is not fused.
struct Layout<'a> {
    chars: Peekable<Chars<'a>>,
    grid: &'a Grid,
    doc: bool,
    /// Doc column width in cells.
    col_w: usize,
    wide_cells: bool,
    black_ink: bool,
    sentence: usize,
    dim: bool,
    /// Grid: next cell index. Doc: next cell within the current line.
    cursor: usize,
    /// Doc: current `'\n'`-separated line.
    line: usize,
}

impl<'a> Layout<'a> {
    fn new(text: &'a str, grid: &'a Grid, doc: bool, wide_cells: bool, black_ink: bool) -> Self {
        Self {
            chars: text.chars().peekable(),
            grid,
            doc,
            col_w: grid.cols.saturating_sub(GUTTER) / 2,
            wide_cells,
            black_ink,
            sentence: 0,
            dim: false,
            cursor: 0,
            line: 0,
        }
    }

    /// No cell can ever be placed, so renderers skip painting entirely.
    const fn is_empty(&self) -> bool {
        if self.doc {
            self.col_w == 0 || self.grid.rows == 0
        } else {
            self.grid.cols * self.grid.rows == 0
        }
    }

    /// Grid rows the text occupies, so the canvas height hugs the content
    /// instead of padding the frame to a full square. Runs the same walk the
    /// renderers draw from; when it stops early (capacity reached, or past the
    /// second doc column) the count already exceeds `grid.rows`, so the clamp
    /// gives the same answer as counting the whole text.
    fn used_rows(mut self) -> usize {
        self.by_ref().for_each(drop);
        let rows = if self.doc {
            self.line + 1
        } else {
            self.cursor.div_ceil(self.grid.cols)
        };
        rows.clamp(1, self.grid.rows)
    }
}

impl Iterator for Layout<'_> {
    type Item = LaidCell;

    fn next(&mut self) -> Option<LaidCell> {
        let grid = self.grid;
        loop {
            if !self.doc && self.cursor >= grid.cols * grid.rows {
                return None;
            }
            let ch = self.chars.next()?;
            let code = ch as u32;
            match code {
                DIM_ON => {
                    self.dim = true;
                    continue;
                }
                DIM_OFF => {
                    self.dim = false;
                    continue;
                }
                0x0a if self.doc => {
                    self.line += 1;
                    self.cursor = 0;
                    if self.line >= grid.rows * 2 {
                        return None;
                    }
                    continue;
                }
                _ => {}
            }
            let ink = if self.dim {
                INK_DIM
            } else if self.black_ink {
                INK_BLACK
            } else {
                (1 + self.sentence % INK_COLORS) as u8
            };
            if matches!(code, 0x2e | 0x21 | 0x3f)
                && self.chars.peek().is_some_and(|&next| {
                    matches!(next as u32, 0x20 | FULL_BLOCK) || (self.doc && next == '\n')
                })
            {
                self.sentence += 1;
            }
            let units = if self.wide_cells && is_wide(code) {
                2
            } else {
                1
            };
            let mut cell = self.cursor;
            if self.doc {
                if units == 2 && self.col_w >= 2 && cell == self.col_w - 1 {
                    cell += 1; // pad: never split a wide glyph across the column edge
                }
                self.cursor = cell + units;
                if self.cursor > self.col_w {
                    continue; // clip past the column width
                }
                let column = self.line / grid.rows;
                let row = self.line - column * grid.rows;
                let x = (column * (self.col_w + GUTTER) + cell) * grid.cell_w;
                return Some(LaidCell {
                    ch,
                    ink,
                    x,
                    row,
                    units,
                });
            }
            if units == 2 && grid.cols >= 2 && cell % grid.cols == grid.cols - 1 {
                cell += 1; // pad: never split a wide glyph across two rows
            }
            self.cursor = cell + units;
            if cell >= grid.cols * grid.rows {
                return None;
            }
            let row = cell / grid.cols;
            let x = (cell - row * grid.cols) * grid.cell_w;
            return Some(LaidCell {
                ch,
                ink,
                x,
                row,
                units,
            });
        }
    }
}

/// Paint the pale highlight bands behind line copies after the first.
fn fill_repeat_bands(pixels: &mut [u8], width: usize, height: usize, grid: &Grid) {
    if grid.repeat <= 1 {
        return;
    }
    for row in 0..grid.rows {
        for copy in 1..grid.repeat {
            let band_top = (row * grid.repeat + copy) * grid.cell_h;
            for y in band_top..(band_top + grid.cell_h).min(height) {
                pixels[y * width..y * width + width].fill(BG_REPEAT);
            }
        }
    }
}

/// Blit one glyph's bitmask rows at (`left`, `top`), clipped to the canvas.
fn blit_glyph(
    pixels: &mut [u8],
    width: usize,
    height: usize,
    glyph: &Glyph,
    left: i32,
    top: i32,
    ink: u8,
) {
    for (r, &bits) in glyph.rows.iter().enumerate() {
        if bits == 0 {
            continue;
        }
        let y = top + r as i32;
        if y < 0 || y >= height as i32 {
            continue;
        }
        let row_base = y as usize * width;
        for b in 0..glyph.w {
            if bits & (0x80u8 >> b) != 0 {
                let x = left + i32::from(b);
                if x >= 0 && (x as usize) < width {
                    pixels[row_base + x as usize] = ink;
                }
            }
        }
    }
}

/// Fill one cell box (every repeat copy) with solid ink, clipped to canvas.
fn fill_cell(
    pixels: &mut [u8],
    width: usize,
    height: usize,
    grid: &Grid,
    x_origin: usize,
    row: usize,
    ink: u8,
) {
    let x0 = x_origin.min(width);
    let x1 = (x_origin + grid.cell_w).min(width);
    if x0 >= x1 {
        return;
    }
    for copy in 0..grid.repeat {
        let top = (row * grid.repeat + copy) * grid.cell_h;
        for y in top..(top + grid.cell_h).min(height) {
            pixels[y * width + x0..y * width + x1].fill(ink);
        }
    }
}

fn fill_repeat_bands_rgb(pixels: &mut [u8], width: usize, height: usize, grid: &Grid) {
    if grid.repeat <= 1 {
        return;
    }
    let band = PALETTE[BG_REPEAT as usize];
    for row in 0..grid.rows {
        for copy in 1..grid.repeat {
            let band_top = (row * grid.repeat + copy) * grid.cell_h;
            for y in band_top..(band_top + grid.cell_h).min(height) {
                for px in pixels[y * width * 3..(y + 1) * width * 3]
                    .as_chunks_mut::<3>()
                    .0
                {
                    px.copy_from_slice(&band);
                }
            }
        }
    }
}

fn fill_cell_rgb(
    pixels: &mut [u8],
    width: usize,
    height: usize,
    grid: &Grid,
    x_origin: usize,
    row: usize,
    ink: u8,
) {
    let x0 = x_origin.min(width);
    let x1 = (x_origin + grid.cell_w).min(width);
    if x0 >= x1 {
        return;
    }
    let color = PALETTE[ink as usize];
    for copy in 0..grid.repeat {
        let top = (row * grid.repeat + copy) * grid.cell_h;
        for y in top..(top + grid.cell_h).min(height) {
            let row = &mut pixels[y * width * 3..(y + 1) * width * 3];
            for x in x0..x1 {
                row[x * 3..x * 3 + 3].copy_from_slice(&color);
            }
        }
    }
}

/// Silver pixel size for a glyph spanning `units` grid cells: scaled to that
/// box so full-width fallback CJK fills its doubled width instead of a single
/// narrow ASCII cell.
fn ttf_pixel_size(grid: &Grid, units: usize) -> f32 {
    let sx = (units * grid.cell_w) as f32 / SILVER_CELL as f32;
    let sy = grid.cell_h as f32 / SILVER_CELL as f32;
    SILVER_PX * sx.min(sy)
}

fn ttf_ascent(font: &TtfFont, px: f32) -> f32 {
    font.face
        .horizontal_line_metrics(px)
        .map_or(font.ascent * px / SILVER_PX, |metrics| metrics.ascent)
}

fn cached_ttf_glyph<'a>(
    cache: &'a mut HashMap<char, RasterizedGlyph>,
    font: &TtfFont,
    ch: char,
    px: f32,
) -> Option<&'a RasterizedGlyph> {
    if font.face.lookup_glyph_index(ch) == 0 {
        return None;
    }
    Some(cache.entry(ch).or_insert_with(|| {
        let (metrics, bitmap) = font.face.rasterize(ch, px);
        RasterizedGlyph { metrics, bitmap }
    }))
}

fn blit_ttf_glyph(
    pixels: &mut [u8],
    width: usize,
    height: usize,
    glyph: &RasterizedGlyph,
    left: i32,
    top: i32,
    ink: u8,
) {
    if glyph.metrics.width == 0 || glyph.metrics.height == 0 {
        return;
    }
    let color = PALETTE[ink as usize];
    for y in 0..glyph.metrics.height {
        let dst_y = top + y as i32;
        if dst_y < 0 || dst_y >= height as i32 {
            continue;
        }
        for x in 0..glyph.metrics.width {
            let alpha = u16::from(glyph.bitmap[y * glyph.metrics.width + x]);
            if alpha == 0 {
                continue;
            }
            let dst_x = left + x as i32;
            if dst_x < 0 || dst_x >= width as i32 {
                continue;
            }
            let offset = (dst_y as usize * width + dst_x as usize) * 3;
            let inv = 255 - alpha;
            for c in 0..3 {
                let bg = u16::from(pixels[offset + c]);
                let fg = u16::from(color[c]);
                pixels[offset + c] = ((bg * inv + fg * alpha + 127) / 255) as u8;
            }
        }
    }
}

fn blit_ttf_glyph_indexed(
    pixels: &mut [u8],
    width: usize,
    height: usize,
    glyph: &RasterizedGlyph,
    left: i32,
    top: i32,
    ink: u8,
) {
    if glyph.metrics.width == 0 || glyph.metrics.height == 0 {
        return;
    }
    for y in 0..glyph.metrics.height {
        let dst_y = top + y as i32;
        if dst_y < 0 || dst_y >= height as i32 {
            continue;
        }
        let row_base = dst_y as usize * width;
        for x in 0..glyph.metrics.width {
            // Two-level anti-alias for the on/off indexed palette: a solid core
            // only where coverage is high, and a single dim-gray fringe on the
            // partially covered edges, so scaled CJK reads lighter than a
            // flat-thresholded (and visibly bold) glyph. Dim spans stay dim.
            let coverage = glyph.bitmap[y * glyph.metrics.width + x];
            let cell = if coverage >= 170 {
                ink
            } else if ink == INK_BLACK && coverage >= 56 {
                INK_DIM // soft gray fringe only on black ink (one neutral palette slot)
            } else if coverage >= 110 {
                ink
            } else {
                continue;
            };
            let dst_x = left + x as i32;
            if dst_x >= 0 && dst_x < width as i32 {
                pixels[row_base + dst_x as usize] = cell;
            }
        }
    }
}

fn ttf_glyph_origin(x_origin: usize, cell_w: usize, metrics: &Metrics) -> i32 {
    let advance = metrics.advance_width.ceil() as i32;
    let pad = (cell_w as i32 - advance).max(0) / 2;
    x_origin as i32 + pad + metrics.xmin
}

fn ttf_glyph_top(cell_top: usize, ascent: f32, metrics: &Metrics) -> i32 {
    (cell_top as f32 + ascent - metrics.height as f32 - metrics.ymin as f32).round() as i32
}

/// Rasterize `text` onto a `width` x `height` palette-indexed bitmap on the
/// grid's cell box, laid out by [`Layout`] (grid, or two doc columns when
/// `doc`). Glyphs keep their natural size with the baseline at the font's
/// ascent from the cell top, so a cell taller than the font pads below the
/// baseline (the "8on16" shapes). Each text line is printed `grid.repeat`
/// times; copies after the first sit on the highlight band. `U+2588` fills
/// its whole cell with pitch-black ink, ignoring hue and dim state. Code
/// points missing from the bitmap font draw through the embedded Silver
/// TrueType fallback when it has a glyph.
fn render_bitmap(
    text: &str,
    width: usize,
    height: usize,
    font: &Font,
    grid: &Grid,
    doc: bool,
    black_ink: bool,
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    let mut pixels = vec![0u8; width * height]; // 0 = white background
    let layout = Layout::new(text, grid, doc, true, black_ink);
    if layout.is_empty() {
        return Ok(pixels);
    }
    fill_repeat_bands(&mut pixels, width, height, grid);
    let mut fallback_cache = HashMap::new();
    for cell in layout {
        check_cancel(cancel)?;
        if cell.ch as u32 == FULL_BLOCK {
            fill_cell(
                &mut pixels,
                width,
                height,
                grid,
                cell.x,
                cell.row,
                INK_BLACK,
            );
            continue;
        }
        if let Some(glyph) = font.glyphs.get(&(cell.ch as u32)) {
            if glyph.rows.is_empty() {
                if cell.ch != ' ' {
                    bail!(
                        "empty bitmap glyph U+{:04X}; archive refused",
                        cell.ch as u32
                    );
                }
                continue;
            }
            let left = cell.x as i32 + glyph.xoff;
            for copy in 0..grid.repeat {
                let cell_top = ((cell.row * grid.repeat + copy) * grid.cell_h) as i32;
                let top = cell_top + font.ascent - glyph.h - glyph.yoff;
                blit_glyph(&mut pixels, width, height, glyph, left, top, cell.ink);
            }
        } else {
            let px = ttf_pixel_size(grid, cell.units);
            let Some(glyph) = cached_ttf_glyph(&mut fallback_cache, &FONT_SILVER, cell.ch, px)
            else {
                bail!(
                    "missing fallback glyph U+{:04X}; archive refused",
                    cell.ch as u32
                );
            };
            let threshold = if cell.ink == INK_BLACK { 56 } else { 110 };
            if cell.ch != ' ' && !glyph.bitmap.iter().any(|coverage| *coverage >= threshold) {
                bail!(
                    "glyph U+{:04X} is invisible at the selected shape's pixel size; archive refused",
                    cell.ch as u32
                );
            }
            let left = ttf_glyph_origin(cell.x, cell.units * grid.cell_w, &glyph.metrics);
            for copy in 0..grid.repeat {
                let cell_top = (cell.row * grid.repeat + copy) * grid.cell_h;
                let top = ttf_glyph_top(cell_top, font.ascent as f32, &glyph.metrics);
                blit_ttf_glyph_indexed(&mut pixels, width, height, glyph, left, top, cell.ink);
            }
        }
    }
    Ok(pixels)
}

/// RGB counterpart of [`render_bitmap`] for the TrueType font: same layout,
/// glyphs alpha-blended from grayscale coverage, one cell per code point.
fn render_ttf_rgb(
    text: &str,
    width: usize,
    height: usize,
    font: &TtfFont,
    grid: &Grid,
    doc: bool,
    black_ink: bool,
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    let mut pixels = vec![255u8; width * height * 3];
    let layout = Layout::new(text, grid, doc, false, black_ink);
    if layout.is_empty() {
        return Ok(pixels);
    }
    fill_repeat_bands_rgb(&mut pixels, width, height, grid);
    let px = ttf_pixel_size(grid, 1);
    let ascent = ttf_ascent(font, px);
    let mut cache = HashMap::new();
    for cell in layout {
        check_cancel(cancel)?;
        if cell.ch as u32 == FULL_BLOCK {
            fill_cell_rgb(
                &mut pixels,
                width,
                height,
                grid,
                cell.x,
                cell.row,
                INK_BLACK,
            );
            continue;
        }
        let Some(glyph) = cached_ttf_glyph(&mut cache, font, cell.ch, px) else {
            bail!(
                "missing Silver glyph U+{:04X}; archive refused",
                cell.ch as u32
            );
        };
        if cell.ch != ' ' && !glyph.bitmap.iter().any(|coverage| *coverage != 0) {
            bail!(
                "glyph U+{:04X} is invisible at the selected pixel size; archive refused",
                cell.ch as u32
            );
        }
        let left = ttf_glyph_origin(cell.x, grid.cell_w, &glyph.metrics);
        for copy in 0..grid.repeat {
            let cell_top = (cell.row * grid.repeat + copy) * grid.cell_h;
            let top = ttf_glyph_top(cell_top, ascent, &glyph.metrics);
            blit_ttf_glyph(&mut pixels, width, height, glyph, left, top, cell.ink);
        }
    }
    Ok(pixels)
}

/// Character cells between the two doc columns (eval `exp14` layout).
const GUTTER: usize = 3;

// ============================================================================
// Lanczos3 resampling (stretch shapes)
// ============================================================================

fn lanczos3(x: f32) -> f32 {
    let x = x.abs();
    if x < 1e-6 {
        return 1.0;
    }
    if x >= 3.0 {
        return 0.0;
    }
    let pix = PI * x;
    (pix.sin() / pix) * ((pix / 3.0).sin() / (pix / 3.0))
}

/// Per-output-pixel kernel contributions for one axis, PIL-convention
/// (`center = (i + 0.5) * scale`, kernel stretched by `max(scale, 1)`,
/// weights normalized).
fn contributions(src_len: usize, dst_len: usize) -> Vec<(usize, Vec<f32>)> {
    let scale = src_len as f32 / dst_len as f32;
    let filt_scale = scale.max(1.0);
    let support = 3.0 * filt_scale;
    let mut out = Vec::with_capacity(dst_len);
    for i in 0..dst_len {
        let center = (i as f32 + 0.5) * scale;
        let begin = ((center - support) as isize).max(0) as usize;
        let end = ((center + support).ceil() as usize).min(src_len);
        let mut weights = Vec::with_capacity(end - begin);
        let mut total = 0.0f32;
        for x in begin..end {
            let w = lanczos3((x as f32 + 0.5 - center) / filt_scale);
            weights.push(w);
            total += w;
        }
        if total != 0.0 {
            for w in &mut weights {
                *w /= total;
            }
        }
        out.push((begin, weights));
    }
    out
}

/// `a * b + c`, fused only when the build targets FMA: without it (`x86-64-v2`
/// baseline) `mul_add` lowers to a scalar libm `fmaf` call per element.
#[inline(always)]
#[allow(
    clippy::suboptimal_flops,
    reason = "`mul_add` is a slow libm call on x86-64 without FMA"
)]
fn madd(a: f32, b: f32, c: f32) -> f32 {
    if cfg!(target_feature = "fma") {
        a.mul_add(b, c)
    } else {
        a * b + c
    }
}

/// Separable Lanczos3 resize of a palette-indexed `sw` x `sh` canvas into the
/// top-left `dw` x `dh` corner of the RGB8 `frame` (row stride `frame_w`
/// pixels). The horizontal pass reads palette colours straight from the
/// indices and the vertical pass accumulates one output row at a time, so the
/// only full-size f32 buffer is the `dw * sh` horizontal intermediate (no f32
/// copy of the source, no f32 copy of the output).
fn stretch_indexed(
    src: &[u8],
    sw: usize,
    sh: usize,
    dw: usize,
    dh: usize,
    frame: &mut [u8],
    frame_w: usize,
    cancel: &AtomicBool,
) -> Result<()> {
    let horiz = contributions(sw, dw);
    let mut tmp = vec![0f32; dw * sh * 3];
    for y in 0..sh {
        check_cancel(cancel)?;
        let src_row = &src[y * sw..(y + 1) * sw];
        let dst_row = &mut tmp[y * dw * 3..(y + 1) * dw * 3];
        for (x, (begin, weights)) in horiz.iter().enumerate() {
            let mut acc = [0f32; 3];
            for (&idx, &w) in src_row[*begin..].iter().zip(weights) {
                let [r, g, b] = PALETTE[idx as usize];
                acc[0] = madd(f32::from(r), w, acc[0]);
                acc[1] = madd(f32::from(g), w, acc[1]);
                acc[2] = madd(f32::from(b), w, acc[2]);
            }
            dst_row[x * 3..x * 3 + 3].copy_from_slice(&acc);
        }
    }
    let vert = contributions(sh, dh);
    let mut acc_row = vec![0f32; dw * 3];
    let copy_len = dw.min(frame_w) * 3;
    for (y, (begin, weights)) in vert.iter().enumerate() {
        check_cancel(cancel)?;
        acc_row.fill(0.0);
        for (k, &w) in weights.iter().enumerate() {
            let src_row = &tmp[(begin + k) * dw * 3..(begin + k + 1) * dw * 3];
            for (d, &s) in acc_row.iter_mut().zip(src_row) {
                *d = madd(s, w, *d);
            }
        }
        let dst_row = &mut frame[y * frame_w * 3..][..copy_len];
        for (d, &s) in dst_row.iter_mut().zip(&acc_row) {
            *d = s.round().clamp(0.0, 255.0) as u8;
        }
    }
    Ok(())
}

// ============================================================================
// PNG encoding
// ============================================================================

/// Pack one-byte-per-pixel palette indices into `bits`-per-pixel PNG
/// scanline data (big-endian within each byte), remapping each global
/// palette index through `remap` to its per-frame slot on the way.
fn pack_bits(
    pixels: &[u8],
    width: usize,
    height: usize,
    bits: usize,
    remap: &[u8; PALETTE.len()],
) -> Vec<u8> {
    let per = 8 / bits;
    let row_bytes = width.div_ceil(per);
    let mut packed = vec![0u8; row_bytes * height];
    for y in 0..height {
        let src = &pixels[y * width..(y + 1) * width];
        let dst = &mut packed[y * row_bytes..(y + 1) * row_bytes];
        for (x, &px) in src.iter().enumerate() {
            dst[x / per] |= remap[px as usize] << (bits * (per - 1 - x % per));
        }
    }
    packed
}

/// Encode a palette-indexed bitmap as an indexed PNG with `None` row
/// filtering (the glyph bitmap is already minimal-entropy; filtering costs
/// encode time without helping deflate).
///
/// The palette is narrowed to the colors the frame actually uses and the bit
/// depth follows: a plain `bw` frame (background + ink) packs 1-bit rows, a
/// dim/banded frame 2-bit, sentence-hue frames 4-bit — bw frames shed
/// another ~half of the pre-deflate stream vs the fixed 4-bit layout.
fn encode_indexed_png(
    pixels: &[u8],
    width: usize,
    height: usize,
    compression: png::Compression,
) -> Result<Vec<u8>> {
    let mut used = [false; PALETTE.len()];
    for &px in pixels {
        used[px as usize] = true;
    }
    let mut remap = [0u8; PALETTE.len()];
    let mut palette = Vec::with_capacity(PALETTE.len() * 3);
    let mut count = 0u8;
    for (global, &is_used) in used.iter().enumerate() {
        if is_used {
            remap[global] = count;
            count += 1;
            palette.extend_from_slice(&PALETTE[global]);
        }
    }
    let (depth, bits) = match count {
        0..=2 => (png::BitDepth::One, 1),
        3..=4 => (png::BitDepth::Two, 2),
        _ => (png::BitDepth::Four, 4),
    };
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width as u32, height as u32);
    encoder.set_color(png::ColorType::Indexed);
    encoder.set_depth(depth);
    encoder.set_palette(Cow::Owned(palette));
    encoder.set_compression(compression);
    // MUST come after `set_compression`, which resets the filter to the
    // compression level's default (`Adaptive` for `Balanced`/`High`).
    encoder.set_filter(png::Filter::NoFilter);
    let mut writer = encoder
        .write_header()
        .map_err(|err| anyhow::anyhow!("Failed to write PNG header: {err}"))?;
    writer
        .write_image_data(&pack_bits(pixels, width, height, bits, &remap))
        .map_err(|err| anyhow::anyhow!("Failed to write PNG data: {err}"))?;
    writer
        .finish()
        .map_err(|err| anyhow::anyhow!("Failed to finish PNG stream: {err}"))?;
    Ok(out)
}

/// Encode an interleaved RGB8 buffer as PNG. Stretched frames are
/// continuous-tone, so adaptive filtering (the `Balanced` default) helps.
fn encode_rgb_png(
    pixels: &[u8],
    width: usize,
    height: usize,
    compression: png::Compression,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width as u32, height as u32);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(compression);
    let mut writer = encoder
        .write_header()
        .map_err(|err| anyhow::anyhow!("Failed to write PNG header: {err}"))?;
    writer
        .write_image_data(pixels)
        .map_err(|err| anyhow::anyhow!("Failed to write PNG data: {err}"))?;
    writer
        .finish()
        .map_err(|err| anyhow::anyhow!("Failed to finish PNG stream: {err}"))?;
    Ok(out)
}

pub(super) fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        bail!("snapcompact interrupted");
    }
    Ok(())
}

pub(super) fn cell_units(ch: char, font: &str) -> usize {
    if matches!(ch as u32, DIM_ON | DIM_OFF) {
        0
    } else if font != "silver" && is_wide(ch as u32) {
        2
    } else {
        1
    }
}

pub(super) fn validate_text(text: &str, name: &str, cancel: &AtomicBool) -> Result<()> {
    let font = resolve_font(name).ok_or_else(|| anyhow::anyhow!("unsupported font {name}"))?;
    // Bundled fonts are immutable (<5 MiB total); no user font files are parsed.
    // Bound the validation/raster glyph working set independently of transcript size.
    let mut glyphs = HashMap::new();
    for (index, ch) in text.chars().enumerate() {
        if index % 1024 == 0 {
            check_cancel(cancel)?;
        }
        if matches!(ch as u32, DIM_ON | DIM_OFF | FULL_BLOCK | 0x0a) {
            continue;
        }
        if !glyphs.contains_key(&ch) && glyphs.len() >= 65_536 {
            bail!("snapcompact distinct glyph limit exceeded; no source was omitted");
        }
        let supported = *glyphs.entry(ch).or_insert_with(|| {
            let bitmap = match &font {
                RenderFont::Bitmap(font) => font.glyphs.get(&(ch as u32)),
                RenderFont::Ttf(_) => None,
            };
            if let Some(glyph) = bitmap {
                return ch == ' ' || glyph.rows.iter().any(|bits| *bits != 0);
            }
            if FONT_SILVER.face.lookup_glyph_index(ch) == 0 {
                return false;
            }
            ch == ' '
                || FONT_SILVER
                    .face
                    .rasterize(ch, SILVER_PX)
                    .1
                    .iter()
                    .any(|coverage| *coverage != 0)
        });
        if !supported || (ch != ' ' && ch.is_control()) {
            bail!(
                "snapcompact font {name} has no printable glyph for U+{:04X} at transcript character {index}; archive refused without replacing or dropping it",
                ch as u32
            );
        }
    }
    Ok(())
}

pub(super) struct Rendered {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub(super) fn render(text: &str, shape: &super::Shape, cancel: &AtomicBool) -> Result<Rendered> {
    check_cancel(cancel)?;
    let size = shape.size as usize;
    if shape.size == 0 || shape.size > MAX_FRAME_SIZE {
        bail!("snapcompact frame size exceeds limit");
    }
    let font = resolve_font(shape.font).ok_or_else(|| anyhow::anyhow!("unsupported font"))?;
    let grid = Grid {
        cols: size / shape.cell_width,
        rows: size / shape.cell_height / shape.repeat,
        repeat: shape.repeat,
        cell_w: shape.cell_width,
        cell_h: shape.cell_height,
    };
    let wide = matches!(font, RenderFont::Bitmap(_));
    let doc = shape.columns == 2;
    // Caller paginates, but this boundary also rejects omissions if called incorrectly.
    let expected = text
        .chars()
        .filter(|ch| !matches!(*ch as u32, DIM_ON | DIM_OFF) && !(doc && *ch == '\n'))
        .count();
    let actual = Layout::new(text, &grid, doc, wide, shape.black).count();
    if expected != actual {
        bail!(
            "snapcompact frame would omit {difference} characters; archive refused",
            difference = expected.saturating_sub(actual)
        );
    }
    let used = Layout::new(text, &grid, doc, wide, shape.black).used_rows();
    let content_height = used * grid.repeat * grid.cell_h;
    let height = content_height.max(64);
    if size
        .checked_mul(height)
        .is_none_or(|pixels| pixels > 7_000_000)
    {
        bail!("snapcompact frame pixel limit exceeded");
    }
    let png = match font {
        RenderFont::Ttf(font) => {
            let mut pixels =
                render_ttf_rgb(text, size, height, font, &grid, doc, shape.black, cancel)?;
            pixels[content_height * size * 3..].fill(255);
            encode_rgb_png(&pixels, size, height, png::Compression::Balanced)?
        }
        RenderFont::Bitmap(font) => {
            if !shape.stretch || (grid.cell_w, grid.cell_h) == (font.cell_w, font.cell_h) {
                let mut pixels =
                    render_bitmap(text, size, height, font, &grid, doc, shape.black, cancel)?;
                pixels[content_height * size..].fill(0);
                encode_indexed_png(&pixels, size, height, png::Compression::Balanced)?
            } else {
                let native = Grid {
                    cell_w: font.cell_w,
                    cell_h: font.cell_h,
                    ..grid
                };
                let sw = grid.cols * font.cell_w;
                let sh = used * grid.repeat * font.cell_h;
                let indexed = render_bitmap(text, sw, sh, font, &native, doc, shape.black, cancel)?;
                let mut frame = vec![255u8; size * height * 3];
                stretch_indexed(
                    &indexed,
                    sw,
                    sh,
                    grid.cols * grid.cell_w,
                    content_height,
                    &mut frame,
                    size,
                    cancel,
                )?;
                encode_rgb_png(&frame, size, height, png::Compression::Balanced)?
            }
        }
    };
    check_cancel(cancel)?;
    if png.len() > 24 * 1024 * 1024 {
        bail!("snapcompact PNG byte limit exceeded");
    }
    Ok(Rendered {
        png,
        width: shape.size,
        height: height as u32,
    })
}
