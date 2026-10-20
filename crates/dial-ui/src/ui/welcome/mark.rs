//! The launch mark: a braille knob beside a geometric braille "dial". Shapes are distance tests
//! sampled on the braille dot grid (2×4 dots per cell), so the needle turns smoothly while the
//! mark settles at start-up: the needle swings up from its stop, the ticks it passes light up and
//! the wordmark tunes in from left to right. Progress `>= 1.0` is the resting frame.
use super::*;

/// Dot offsets of a braille cell with their bit in U+2800.
const DOTS: [(usize, usize, u32); 8] = [
    (0, 0, 0x01),
    (0, 1, 0x02),
    (0, 2, 0x04),
    (1, 0, 0x08),
    (1, 1, 0x10),
    (1, 2, 0x20),
    (0, 3, 0x40),
    (1, 3, 0x80),
];

/// Knob canvas in dots and its center.
const KNOB: (usize, usize) = (34, 28);
const CENTER: (f32, f32) = (17.0, 16.0);
/// The knob's travel: from the lower-left stop, clockwise over the top, 270°.
const START: f32 = 135.0;
const ARC: f32 = 270.0;
/// Where the needle rests, as a fraction of its travel.
const REST: f32 = 0.62;
const TICKS: usize = 9;
/// Wordmark canvas in dots.
const WORD: (usize, usize) = (38, 20);
/// Cells between the knob and the wordmark, and the wordmark's row offset beside the knob.
const GAP: usize = 3;
const WORD_TOP: usize = 1;

/// Cells of the knob-and-wordmark lockup: `(width, height)`.
pub(super) const LOCKUP: (usize, usize) = (KNOB.0 / 2 + GAP + WORD.0 / 2, KNOB.1 / 4);
/// Cells of the wordmark alone.
pub(super) const WORDMARK: (usize, usize) = (WORD.0 / 2, WORD.1 / 4);

/// What a dot belongs to, in drawing priority: a cell takes the color of its strongest dot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Ink {
    Idle,
    Ring,
    Lit,
    Needle,
}

type Cells = Vec<Vec<(char, Option<Ink>)>>;

fn segment(x: f32, y: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = dx * dx + dy * dy;
    let t = if len == 0.0 {
        0.0
    } else {
        (((x - a.0) * dx + (y - a.1) * dy) / len).clamp(0.0, 1.0)
    };
    (x - a.0 - t * dx).hypot(y - a.1 - t * dy)
}

/// The point at radius `r` and screen angle `deg` (clockwise from +x, y grows downward).
fn polar(r: f32, deg: f32) -> (f32, f32) {
    let a = deg.to_radians();
    (CENTER.0 + r * a.cos(), CENTER.1 + r * a.sin())
}

/// How far the mark has settled: an ease-out with a small overshoot, then exactly `1.0`.
fn level(progress: f32) -> f32 {
    if !(0.0..1.0).contains(&progress) {
        return 1.0;
    }
    const C1: f32 = 1.4;
    let t = progress - 1.0;
    1.0 + (C1 + 1.0) * t.powi(3) + C1 * t.powi(2)
}

fn knob(x: f32, y: f32, level: f32) -> Option<Ink> {
    let (dx, dy) = (x - CENTER.0, y - CENTER.1);
    let r = dx.hypot(dy);
    let reach = ARC * REST * level;
    let needle = START + reach;
    // A hollow hub and a one-dot needle: thin strokes stay crisp at braille resolution.
    if (r - 2.0).abs() <= 0.75 || segment(x, y, polar(3.2, needle), polar(10.0, needle)) <= 0.6 {
        return Some(Ink::Needle);
    }
    // Position along the travel; the opening at the bottom lies beyond `ARC`.
    let along = (dy.atan2(dx).to_degrees() - START).rem_euclid(360.0);
    if (r - 14.5).abs() <= 1.0 && along <= ARC {
        return Some(Ink::Ring);
    }
    (0..TICKS).find_map(|k| {
        let at = ARC * k as f32 / (TICKS - 1) as f32;
        (segment(x, y, polar(8.5, START + at), polar(10.5, START + at)) <= 0.75).then_some(
            if at <= reach + 0.5 {
                Ink::Lit
            } else {
                Ink::Idle
            },
        )
    })
}

/// Geometric lowercase "dial": circular bowls, straight stems, a round tittle.
fn word(x: f32, y: f32) -> Option<Ink> {
    let bowl = |cx: f32| ((x - cx).hypot(y - 13.0) - 5.5).abs() <= 1.0;
    let stem = |sx: f32, top: f32| segment(x, y, (sx, top), (sx, 18.5)) <= 1.0;
    let hit = bowl(6.5)
        || stem(12.0, 1.5)
        || stem(16.0, 7.5)
        || (x - 16.0).hypot(y - 3.5) <= 1.4
        || bowl(25.5)
        || stem(31.0, 7.5)
        || stem(35.0, 1.5);
    hit.then_some(Ink::Ring)
}

fn cells((w, h): (usize, usize), ink: impl Fn(f32, f32) -> Option<Ink>) -> Cells {
    (0..h.div_ceil(4))
        .map(|row| {
            (0..w.div_ceil(2))
                .map(|col| {
                    let (mut bits, mut top) = (0, None);
                    for (dx, dy, bit) in DOTS {
                        let x = (col * 2 + dx) as f32 + 0.5;
                        let y = (row * 4 + dy) as f32 + 0.5;
                        if let Some(found) = ink(x, y) {
                            bits |= bit;
                            top = top.max(Some(found));
                        }
                    }
                    let glyph = char::from_u32(0x2800 + bits).filter(|_| bits != 0);
                    (glyph.unwrap_or(' '), top)
                })
                .collect()
        })
        .collect()
}

/// Styled rows of `grid`. Columns share one brand gradient across `total` cells starting at
/// `first`, so the knob flows into the wordmark; idle dots stay dim and the needle bright.
fn paint(theme: &Theme, grid: &Cells, first: usize, total: usize) -> Vec<Vec<Span<'static>>> {
    let span = total.max(2) as f32 - 1.0;
    grid.iter()
        .map(|row| {
            row.iter()
                .enumerate()
                .map(|(col, &(glyph, ink))| {
                    let style = match ink {
                        None => Style::default(),
                        Some(Ink::Idle) => theme.fg(Tok::Dim),
                        Some(Ink::Needle) => theme.fg(Tok::Text),
                        Some(Ink::Ring | Ink::Lit) => Style::default()
                            .fg(fx::brand_at(theme, (first + col) as f32 / span * 0.5)),
                    };
                    Span::styled(glyph.to_string(), style)
                })
                .collect()
        })
        .collect()
}

fn wordmark_cells(level: f32) -> Cells {
    let tuned = level.min(1.0) * WORD.0 as f32;
    cells(WORD, |x, y| {
        word(x, y).map(|ink| if x <= tuned { ink } else { Ink::Idle })
    })
}

/// The knob and wordmark side by side, exactly [`LOCKUP`] cells.
pub(super) fn lockup(theme: &Theme, progress: f32) -> Vec<Vec<Span<'static>>> {
    let level = level(progress);
    let knob = paint(theme, &cells(KNOB, |x, y| knob(x, y, level)), 0, LOCKUP.0);
    let word = paint(theme, &wordmark_cells(level), KNOB.0 / 2 + GAP, LOCKUP.0);
    knob.into_iter()
        .enumerate()
        .map(|(row, mut spans)| {
            spans.push(Span::raw(" ".repeat(GAP)));
            match row.checked_sub(WORD_TOP).and_then(|r| word.get(r)) {
                Some(cells) => spans.extend(cells.iter().cloned()),
                None => spans.push(Span::raw(" ".repeat(WORDMARK.0))),
            }
            spans
        })
        .collect()
}

/// The wordmark alone, exactly [`WORDMARK`] cells, for windows too small for the knob.
pub(super) fn wordmark(theme: &Theme, progress: f32) -> Vec<Vec<Span<'static>>> {
    paint(theme, &wordmark_cells(level(progress)), 0, WORDMARK.0)
}
