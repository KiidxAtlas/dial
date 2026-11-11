//! The launch mark: a braille knob beside a geometric braille "dial". Shapes are distance tests
//! sampled on the braille dot grid (2×4 dots per cell), so the needle turns smoothly while the
//! mark powers up at start-up: the bezel traces itself in clockwise behind a bright comet, the
//! needle swings up from its stop as the trace passes, the ticks it passes light up, and the
//! wordmark tunes in from left to right under a travelling glow. Progress `>= 1.0` is the resting
//! frame. After that an optional ambient clock keeps the mark quietly alive: the needle breathes
//! and a glint of light crosses now and then.
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

/// Share of the start-up spent tracing the bezel, and when the needle starts to follow it.
const TRACE_SPAN: f32 = 0.55;
const NEEDLE_DELAY: f32 = 0.2;
/// Degrees of bezel behind the trace head drawn bright, like a comet's tail.
const COMET: f32 = 34.0;

/// `(trace, level)` at start-up `progress`: how much of the bezel is drawn (`0..=1`) and how far
/// the needle has turned. Both are exactly `1.0` from `1.0` on, so the resting frame is unchanged.
fn phases(progress: f32) -> (f32, f32) {
    if !(0.0..1.0).contains(&progress) {
        return (1.0, 1.0);
    }
    let t = (progress / TRACE_SPAN).clamp(0.0, 1.0);
    let trace = 1.0 - (1.0 - t).powi(3);
    let needle = ((progress - NEEDLE_DELAY) / (1.0 - NEEDLE_DELAY)).max(0.0);
    (trace, level(needle).max(0.0))
}

fn knob(x: f32, y: f32, level: f32, sway: f32, trace: f32) -> Option<Ink> {
    let (dx, dy) = (x - CENTER.0, y - CENTER.1);
    let r = dx.hypot(dy);
    let reach = ARC * REST * level + sway;
    let needle = START + reach;
    // A hollow hub and a one-dot needle: thin strokes stay crisp at braille resolution.
    if (r - 2.0).abs() <= 0.75 || segment(x, y, polar(3.2, needle), polar(10.0, needle)) <= 0.6 {
        return Some(Ink::Needle);
    }
    // Position along the travel; the opening at the bottom lies beyond `ARC`.
    let along = (dy.atan2(dx).to_degrees() - START).rem_euclid(360.0);
    let head = ARC * trace;
    if (r - 14.5).abs() <= 1.0 && along <= ARC.min(head) {
        // While the bezel traces in, its leading stretch burns bright.
        return Some(if trace < 1.0 && along >= head - COMET {
            Ink::Needle
        } else {
            Ink::Ring
        });
    }
    (0..TICKS).find_map(|k| {
        let at = ARC * k as f32 / (TICKS - 1) as f32;
        (at <= head && segment(x, y, polar(8.5, START + at), polar(10.5, START + at)) <= 0.75)
            .then_some(if at <= reach + 0.5 {
                Ink::Lit
            } else {
                Ink::Idle
            })
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

/// Width of the glow that rides the wordmark's tuning edge during start-up.
const TUNE_BAND: f32 = 3.0;

/// The cell the wordmark is tuned up to at start-up `level`, while it is still tuning.
fn tuning_edge(progress: f32, level: f32) -> Option<f32> {
    (0.0..1.0)
        .contains(&progress)
        .then(|| level.clamp(0.0, 1.0) * WORDMARK.0 as f32)
}

/// A wordmark-sized glint of light crosses the mark once per [`GLINT_PERIOD`] ambient ticks.
const GLINT_PERIOD: u64 = 96;
/// How many ambient ticks one crossing takes; the rest of the period is still.
const GLINT_RUN: u64 = 28;
const GLINT_BAND: f32 = 3.5;
/// How far the resting needle sways either way, in degrees, and the ticks in one slow breath.
const SWAY: f32 = 5.0;
const BREATH: f32 = 48.0;

/// The needle's drift from its resting angle after `idle` ambient ticks: zero at tick 0, so the
/// resting frame is exactly the settled one, then a slow sine that now and then nudges the needle
/// across the next tick mark.
fn sway(idle: u64) -> f32 {
    SWAY * (idle as f32 / BREATH * std::f32::consts::TAU).sin()
}

/// Where the glint's head is after `idle` ambient ticks, in cells from the mark's left edge, or
/// `None` while it is between crossings (always at tick 0).
fn glint(idle: u64, cells: usize) -> Option<f32> {
    let at = (idle % GLINT_PERIOD).checked_sub(GLINT_PERIOD - GLINT_RUN)?;
    let along = (at + 1) as f32 / GLINT_RUN as f32;
    Some(along * (cells as f32 + 2.0 * GLINT_BAND) - GLINT_BAND)
}

/// Lights the columns near `head` (cells from this block's left edge) in every row.
fn shine(theme: &Theme, rows: &mut [Vec<Span<'static>>], head: Option<f32>) {
    if let Some(head) = head {
        for row in rows {
            fx::brighten(theme, row, head, GLINT_BAND);
        }
    }
}

/// The knob and wordmark side by side, exactly [`LOCKUP`] cells. `idle` counts the slow ambient
/// ticks since the mark settled: the needle sways and a glint crosses; `0` is the resting frame.
pub(super) fn lockup(theme: &Theme, progress: f32, idle: u64) -> Vec<Vec<Span<'static>>> {
    let (trace, level) = phases(progress);
    let sway = sway(idle);
    let head = glint(idle, LOCKUP.0);
    let mut knob = paint(
        theme,
        &cells(KNOB, |x, y| knob(x, y, level, sway, trace)),
        0,
        LOCKUP.0,
    );
    let mut word = paint(theme, &wordmark_cells(level), KNOB.0 / 2 + GAP, LOCKUP.0);
    shine(theme, &mut knob, head);
    shine(
        theme,
        &mut word,
        head.map(|head| head - (KNOB.0 / 2 + GAP) as f32),
    );
    if let Some(edge) = tuning_edge(progress, level) {
        for row in &mut word {
            fx::brighten(theme, row, edge, TUNE_BAND);
        }
    }
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

/// The wordmark alone, exactly [`WORDMARK`] cells, for windows too small for the knob. Its glint
/// follows the same `idle` clock as [`lockup`], and it tunes in under the same glow.
pub(super) fn wordmark(theme: &Theme, progress: f32, idle: u64) -> Vec<Vec<Span<'static>>> {
    let (_, level) = phases(progress);
    let mut word = paint(theme, &wordmark_cells(level), 0, WORDMARK.0);
    shine(theme, &mut word, glint(idle, WORDMARK.0));
    if let Some(edge) = tuning_edge(progress, level) {
        for row in &mut word {
            fx::brighten(theme, row, edge, TUNE_BAND);
        }
    }
    word
}

#[cfg(test)]
pub(super) fn boot_phases(progress: f32) -> (f32, f32) {
    phases(progress)
}
