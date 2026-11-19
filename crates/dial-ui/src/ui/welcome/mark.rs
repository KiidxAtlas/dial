//! The launch mark: a gauge knob beside a geometric lowercase "dial", drawn in half-block pixels.
//! Each cell holds two square pixels (`▀` with a foreground and a background color), so strokes
//! are solid and the brand gradient flows smoothly across the whole lockup instead of breaking
//! into scattered braille dots. Shapes are distance tests supersampled on the pixel grid.
//!
//! The mark powers up at start-up: the bezel traces itself in clockwise behind a bright comet,
//! the needle swings up from its stop as the trace passes and lights the arc behind it, and the
//! wordmark tunes in from left to right under a travelling glow. Progress `>= 1.0` is the resting
//! frame. After that an optional ambient clock keeps the mark quietly alive: the needle breathes
//! and a glint of light crosses now and then.
use super::*;
use crate::ui::fx::blend;
use ratatui::style::Color;

/// Knob canvas in pixels (one cell is one pixel wide and two tall) and its center.
const KNOB: (usize, usize) = (17, 16);
const CENTER: (f32, f32) = (8.5, 8.0);
/// Inner and outer radius of the bezel arc.
const BEZEL: (f32, f32) = (6.3, 8.0);
/// The knob's travel: from the lower-left stop, clockwise over the top, 270°.
const START: f32 = 135.0;
const ARC: f32 = 270.0;
/// Where the needle rests, as a fraction of its travel.
const REST: f32 = 0.62;
/// Wordmark canvas in pixels.
const WORD: (usize, usize) = (33, 14);
/// Cells between the knob and the wordmark, and the wordmark's row offset beside the knob.
const GAP: usize = 3;
const WORD_TOP: usize = 1;
/// Supersamples per pixel side: a pixel is set when at least half of its samples hit.
const SAMPLES: usize = 4;

/// Cells of the knob-and-wordmark lockup: `(width, height)`.
pub(super) const LOCKUP: (usize, usize) = (KNOB.0 + GAP + WORD.0, KNOB.1 / 2);
/// Cells of the wordmark alone.
pub(super) const WORDMARK: (usize, usize) = (WORD.0, WORD.1 / 2);

/// What a pixel belongs to: the unlit track, the lit brand gradient, or the bright needle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ink {
    Idle,
    Lit,
    Needle,
}

type Pixels = Vec<Vec<Option<Ink>>>;

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
    // A small hub and a slim needle that stops short of the bezel, so the two never merge.
    if r <= 1.25 || segment(x, y, polar(1.0, needle), polar(4.9, needle)) <= 0.62 {
        return Some(Ink::Needle);
    }
    // Position along the travel; the opening at the bottom lies beyond `ARC`.
    let along = (dy.atan2(dx).to_degrees() - START).rem_euclid(360.0);
    let head = ARC * trace;
    if !(BEZEL.0..=BEZEL.1).contains(&r) || along > ARC.min(head) {
        return None;
    }
    // While the bezel traces in, its leading stretch burns bright; at rest the arc is lit up to
    // the needle, like a level meter, and the rest of the track stays dim.
    Some(if trace < 1.0 && along >= head - COMET {
        Ink::Needle
    } else if along <= reach {
        Ink::Lit
    } else {
        Ink::Idle
    })
}

/// Geometric lowercase "dial": round bowls, two-pixel stems on whole pixels, a square tittle.
fn word(x: f32, y: f32) -> bool {
    const HALF: f32 = 1.05;
    let bowl = |cx: f32| ((x - cx).hypot(y - 10.5) - 3.7).abs() <= HALF;
    let stem = |sx: f32, top: f32| segment(x, y, (sx, top), (sx, 14.3)) <= HALF;
    bowl(4.6)
        || stem(9.0, 1.7)
        || stem(14.0, 7.2)
        || ((13.0..=15.0).contains(&x) && (2.0..=4.0).contains(&y))
        || bowl(21.5)
        || stem(26.0, 7.2)
        || stem(31.0, 1.7)
}

/// Supersampled pixels of a `(w, h)` canvas: a pixel takes the ink most of its samples agree on
/// being set, keeping the strongest ink (the needle over the arc) when several touch it.
fn pixels((w, h): (usize, usize), ink: impl Fn(f32, f32) -> Option<Ink>) -> Pixels {
    let rank = |ink: Ink| match ink {
        Ink::Idle => 0,
        Ink::Lit => 1,
        Ink::Needle => 2,
    };
    (0..h)
        .map(|py| {
            (0..w)
                .map(|px| {
                    let (mut hits, mut top) = (0, None::<Ink>);
                    for sy in 0..SAMPLES {
                        for sx in 0..SAMPLES {
                            let x = px as f32 + (sx as f32 + 0.5) / SAMPLES as f32;
                            let y = py as f32 + (sy as f32 + 0.5) / SAMPLES as f32;
                            if let Some(found) = ink(x, y) {
                                hits += 1;
                                if top.is_none_or(|t| rank(found) > rank(t)) {
                                    top = Some(found);
                                }
                            }
                        }
                    }
                    top.filter(|_| hits * 2 >= SAMPLES * SAMPLES)
                })
                .collect()
        })
        .collect()
}

/// How the pixels of one block are colored: where the block starts in the shared gradient, how
/// far down the lockup it sits, and the columns lit by a glint or the tuning glow.
struct Paint<'a> {
    theme: &'a Theme,
    /// First column of this block and the lockup width the gradient spans.
    first: usize,
    total: usize,
    /// Pixel rows above this block, so the sheen lines up across blocks.
    top: usize,
    /// `(head, band)` glows in this block's own columns.
    glows: Vec<(f32, f32)>,
}

impl Paint<'_> {
    fn color(&self, ink: Ink, col: usize, row: usize) -> Color {
        let theme = self.theme;
        let text = theme.color(Tok::Text).unwrap_or(Color::Reset);
        let base = match ink {
            Ink::Idle => theme.color(Tok::Border).unwrap_or(Color::DarkGray),
            Ink::Needle => text,
            Ink::Lit => {
                let span = self.total.max(2) as f32 - 1.0;
                let lit = fx::brand_at(theme, (self.first + col) as f32 / span * 0.5);
                // A soft sheen: the top of the mark catches a little light on dark themes.
                let y = (self.top + row) as f32 / KNOB.1 as f32;
                let sheen = if theme.is_dark() {
                    (0.28 - y * 0.4).max(0.0)
                } else {
                    0.0
                };
                blend(lit, Color::Rgb(255, 255, 255), sheen)
            }
        };
        let lift = self
            .glows
            .iter()
            .map(|&(head, band)| {
                let distance = (col as f32 - head).abs();
                if distance >= band {
                    0.0
                } else {
                    (1.0 - distance / band).powi(2) * 0.85
                }
            })
            .fold(0.0, f32::max);
        if lift > 0.0 && ink != Ink::Needle {
            blend(base, text, lift)
        } else {
            base
        }
    }

    /// Two pixel rows per cell row: `▀` carries the top pixel as foreground and the bottom one as
    /// background, `▄` a lone bottom pixel, so empty pixels never paint the terminal background.
    fn rows(&self, grid: &Pixels) -> Vec<Vec<Span<'static>>> {
        grid.chunks(2)
            .enumerate()
            .map(|(cell_row, pair)| {
                (0..pair[0].len())
                    .map(|col| {
                        let pixel = |dy: usize| {
                            pair.get(dy)
                                .and_then(|row| row[col])
                                .map(|ink| self.color(ink, col, cell_row * 2 + dy))
                        };
                        match (pixel(0), pixel(1)) {
                            (None, None) => Span::raw(" "),
                            (Some(top), None) => Span::styled("▀", Style::default().fg(top)),
                            (None, Some(bottom)) => Span::styled("▄", Style::default().fg(bottom)),
                            (Some(top), Some(bottom)) if top == bottom => {
                                Span::styled("█", Style::default().fg(top))
                            }
                            (Some(top), Some(bottom)) => {
                                Span::styled("▀", Style::default().fg(top).bg(bottom))
                            }
                        }
                    })
                    .collect()
            })
            .collect()
    }
}

fn wordmark_pixels(level: f32) -> Pixels {
    let tuned = level.min(1.0) * WORD.0 as f32;
    pixels(WORD, |x, y| {
        word(x, y).then_some(if x <= tuned { Ink::Lit } else { Ink::Idle })
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

/// A glint of light crosses the mark once per [`GLINT_PERIOD`] ambient ticks.
const GLINT_PERIOD: u64 = 96;
/// How many ambient ticks one crossing takes; the rest of the period is still.
const GLINT_RUN: u64 = 28;
const GLINT_BAND: f32 = 3.5;
/// How far the resting needle sways either way, in degrees, and the ticks in one slow breath.
const SWAY: f32 = 5.0;
const BREATH: f32 = 48.0;

/// The needle's drift from its resting angle after `idle` ambient ticks: zero at tick 0, so the
/// resting frame is exactly the settled one, then a slow sine.
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

/// The knob and wordmark side by side, exactly [`LOCKUP`] cells. `idle` counts the slow ambient
/// ticks since the mark settled: the needle sways and a glint crosses; `0` is the resting frame.
pub(super) fn lockup(theme: &Theme, progress: f32, idle: u64) -> Vec<Vec<Span<'static>>> {
    let (trace, level) = phases(progress);
    let sway = sway(idle);
    let head = glint(idle, LOCKUP.0);
    let word_left = KNOB.0 + GAP;
    let knob = Paint {
        theme,
        first: 0,
        total: LOCKUP.0,
        top: 0,
        glows: head.map(|h| (h, GLINT_BAND)).into_iter().collect(),
    }
    .rows(&pixels(KNOB, |x, y| knob(x, y, level, sway, trace)));
    let mut glows: Vec<(f32, f32)> = head
        .map(|h| (h - word_left as f32, GLINT_BAND))
        .into_iter()
        .collect();
    glows.extend(tuning_edge(progress, level).map(|edge| (edge, TUNE_BAND)));
    let word = Paint {
        theme,
        first: word_left,
        total: LOCKUP.0,
        top: WORD_TOP * 2,
        glows,
    }
    .rows(&wordmark_pixels(level));
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
    let mut glows: Vec<(f32, f32)> = glint(idle, WORDMARK.0)
        .map(|h| (h, GLINT_BAND))
        .into_iter()
        .collect();
    glows.extend(tuning_edge(progress, level).map(|edge| (edge, TUNE_BAND)));
    Paint {
        theme,
        first: 0,
        total: WORDMARK.0,
        top: 0,
        glows,
    }
    .rows(&wordmark_pixels(level))
}

#[cfg(test)]
pub(super) fn boot_phases(progress: f32) -> (f32, f32) {
    phases(progress)
}

/// Set pixels in rendered mark rows: `█` and a two-colored `▀` hold two, `▀`/`▄` alone one.
#[cfg(test)]
pub(super) fn pixel_count(rows: &[Vec<Span<'static>>]) -> u32 {
    rows.iter()
        .flatten()
        .map(|s| match s.content.as_ref() {
            "█" => 2,
            "▀" if s.style.bg.is_some() => 2,
            "▀" | "▄" => 1,
            _ => 0,
        })
        .sum()
}
