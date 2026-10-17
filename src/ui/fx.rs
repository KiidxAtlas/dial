//! Color effects shared by the welcome wordmark, the working status and turn markers.
//!
//! Everything here degrades instead of failing: a theme whose brand stops are not truecolor (an
//! indexed palette, `mono`, `NO_COLOR`) gets the start stop as one solid color, and ASCII glyph
//! sets are untouched because effects only change colors, never characters. Widths never change,
//! so callers can lay text out before styling it.
use super::theme::{Theme, Tok};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Span,
};
use std::time::Duration;

/// How long the start-up sweep takes to cross the wordmark.
pub const SWEEP: Duration = Duration::from_millis(700);
/// How long a panel takes to reveal itself when it opens.
pub const REVEAL: Duration = Duration::from_millis(140);
/// Frame interval while a motion is running (the idle loop otherwise polls far more slowly).
pub const FRAME: Duration = Duration::from_millis(16);

/// Whether anything may move: the user's `ui.motion` setting, and a theme that can show it. ASCII
/// glyph sets and themes without truecolor brand stops (indexed palettes, `mono`, `NO_COLOR`) stay
/// still, because a sweep there would only blink.
pub fn motion(enabled: bool, theme: &Theme) -> bool {
    enabled && !theme.ascii() && stops(theme).is_some()
}

/// `elapsed / span` clamped to `0.0..=1.0`; a zero span is already complete.
pub fn progress(elapsed: Duration, span: Duration) -> f32 {
    if span.is_zero() {
        return 1.0;
    }
    (elapsed.as_secs_f32() / span.as_secs_f32()).clamp(0.0, 1.0)
}

/// Cubic ease-out: fast start, gentle landing.
fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// Linear blend of two truecolor values; `t` in `0.0..=1.0`.
fn mix(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let t = t.clamp(0.0, 1.0);
    let lerp = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    (lerp(a.0, b.0), lerp(a.1, b.1), lerp(a.2, b.2))
}

fn rgb(color: Option<Color>) -> Option<(u8, u8, u8)> {
    match color {
        Some(Color::Rgb(r, g, b)) => Some((r, g, b)),
        _ => None,
    }
}

/// The theme's three brand stops as truecolor, or `None` when any of them is not truecolor.
fn stops(theme: &Theme) -> Option<[(u8, u8, u8); 3]> {
    Some([
        rgb(theme.color(Tok::BrandStart))?,
        rgb(theme.color(Tok::BrandMid))?,
        rgb(theme.color(Tok::BrandEnd))?,
    ])
}

/// The brand gradient sampled at `t` (`0.0` start, `0.5` middle, `1.0` end; values wrap so an
/// animation can scroll through it). Falls back to the start stop's own style.
pub fn brand_at(theme: &Theme, t: f32) -> Color {
    let Some([a, b, c]) = stops(theme) else {
        return theme.color(Tok::BrandStart).unwrap_or(Color::Reset);
    };
    let t = t.rem_euclid(1.0);
    // Ping-pong so a scrolling gradient has no seam: start → mid → end → mid → start.
    let t = if t <= 0.5 { t * 2.0 } else { 2.0 - t * 2.0 };
    let (r, g, b) = if t <= 0.5 {
        mix(a, b, t * 2.0)
    } else {
        mix(b, c, (t - 0.5) * 2.0)
    };
    Color::Rgb(r, g, b)
}

/// `text` with the brand gradient spread across its visible characters, shifted by `phase`
/// (`0.0` for a static rendering). Spaces keep the base style so no background is painted.
pub fn gradient(theme: &Theme, text: &str, phase: f32, base: Style) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    if stops(theme).is_none() {
        return vec![Span::styled(
            text.to_owned(),
            base.fg(theme.color(Tok::BrandStart).unwrap_or(Color::Reset)),
        )];
    }
    let n = chars.len().max(2) as f32 - 1.0;
    chars
        .iter()
        .enumerate()
        .map(|(i, ch)| {
            // Half the ping-pong range per line keeps a short word from cycling back on itself.
            let t = phase + (i as f32 / n) * 0.5;
            Span::styled(ch.to_string(), base.fg(brand_at(theme, t)))
        })
        .collect()
}

/// The start-up sweep: [`gradient`] with a bright band that crosses the text once as `progress`
/// runs from `0.0` to `1.0`, like a needle settling on a station. Outside that range, and without
/// truecolor stops, this is exactly [`gradient`], so the resting frame never changes.
pub fn sweep(
    theme: &Theme,
    text: &str,
    phase: f32,
    progress: f32,
    base: Style,
) -> Vec<Span<'static>> {
    const BAND: f32 = 4.0;
    let mut spans = gradient(theme, text, phase, base);
    if !(0.0..1.0).contains(&progress) || stops(theme).is_none() {
        return spans;
    }
    let peak = rgb(theme.color(Tok::Text)).unwrap_or((255, 255, 255));
    let n = spans.len() as f32;
    let head = ease_out(progress) * (n + 2.0 * BAND) - BAND;
    for (i, span) in spans.iter_mut().enumerate() {
        let distance = (i as f32 - head).abs();
        if distance >= BAND {
            continue;
        }
        let Some(color) = rgb(span.style.fg) else {
            continue;
        };
        let lift = (1.0 - distance / BAND).powi(2) * 0.85;
        let (r, g, b) = mix(color, peak, lift);
        span.style = span.style.fg(Color::Rgb(r, g, b));
        if distance < 1.0 {
            span.style = span.style.add_modifier(Modifier::BOLD);
        }
    }
    spans
}

/// Rows of a `height`-row panel shown at `progress` through its reveal (eased, at least one).
pub fn reveal_rows(height: u16, progress: f32) -> u16 {
    if progress >= 1.0 || height == 0 {
        return height;
    }
    ((f32::from(height) * ease_out(progress)).ceil() as u16).clamp(1, height)
}

/// A panel opening: rows of `area` below the revealed part are blanked, so the panel unrolls from
/// the top over [`REVEAL`]. A complete reveal leaves the buffer untouched.
pub fn reveal(buf: &mut Buffer, area: Rect, progress: f32) {
    let area = area.intersection(buf.area);
    let shown = reveal_rows(area.height, progress);
    for y in area.top() + shown..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)].reset();
        }
    }
}

/// A shimmer for "working" labels: the text in `base`, with a soft three-character band in the
/// brand color that sweeps left to right as `tick` advances (~12 ticks per second), then rests.
#[cfg(test)]
pub fn shimmer(theme: &Theme, text: &str, tick: u64, base: Style) -> Vec<Span<'static>> {
    let (peak, glow) = shimmer_styles(theme);
    shimmer_with(text, tick, base, peak, glow)
}

/// The band's two styles: the bright center and its softer edges.
pub fn shimmer_styles(theme: &Theme) -> (Style, Style) {
    (
        theme.fg(Tok::BrandStart).add_modifier(Modifier::BOLD),
        theme.fg(Tok::BrandMid),
    )
}

/// [`shimmer`] with the band styles given, for callers that render after the theme is gone.
pub fn shimmer_with(
    text: &str,
    tick: u64,
    base: Style,
    peak: Style,
    glow: Style,
) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    // One sweep across the text plus a short pause, so the band is calm rather than frantic.
    let period = chars.len() as u64 + 8;
    let head = (tick % period) as i64 - 2;
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_style = base;
    for (i, ch) in chars.iter().enumerate() {
        let style = match (i as i64 - head).abs() {
            0 => base.patch(peak),
            1 => base.patch(glow),
            _ => base,
        };
        if style != run_style && !run.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut run), run_style));
        }
        run_style = style;
        run.push(*ch);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, run_style));
    }
    spans
}

/// A color that breathes between the brand start and middle stops over ~2 seconds, for a marker
/// that should read as alive (the spinner glyph, the pending tool mark).
pub fn pulse(theme: &Theme, tick: u64) -> Style {
    let t = (tick % 24) as f32 / 24.0;
    theme.fg(Tok::BrandStart).fg(brand_at(theme, t * 0.5))
}

/// A fixed-width activity track. Known prompt progress fills monotonically; otherwise a
/// small band travels back and forth. It is an activity cue, never a fabricated percentage.
pub fn activity_track(theme: &Theme, tick: u64, fraction: Option<f64>) -> Vec<Span<'static>> {
    const CELLS: usize = 8;
    let filled = fraction
        .filter(|f| f.is_finite())
        .map(|f| (f.clamp(0.0, 1.0) * CELLS as f64).floor() as usize);
    let position = (tick / 2 % 14) as usize;
    let head = if position < CELLS {
        position
    } else {
        14 - position
    };
    (0..CELLS)
        .map(|i| {
            let active = filled.map_or(i == head, |n| i < n);
            let glyph = match (theme.ascii(), active) {
                (true, true) => "=",
                (true, false) => "-",
                (false, true) => "━",
                (false, false) => "─",
            };
            Span::styled(
                glyph,
                theme.fg(if active { Tok::Accent } else { Tok::Border }),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::text;

    fn width(spans: &[Span]) -> usize {
        spans.iter().map(|s| text::width(&s.content)).sum()
    }

    #[test]
    fn activity_track_moves_only_when_progress_is_unknown_and_keeps_its_width() {
        for theme in [
            Theme::dark(),
            Theme::light(),
            Theme::mono(),
            Theme::dark().with_glyphs(crate::ui::theme::Glyphs::Ascii),
        ] {
            assert_ne!(
                activity_track(&theme, 0, None),
                activity_track(&theme, 6, None)
            );
            for fraction in [Some(0.0), Some(0.5), Some(1.0)] {
                assert_eq!(
                    activity_track(&theme, 0, fraction),
                    activity_track(&theme, 6, fraction)
                );
            }
            for tick in 0..60 {
                for fraction in [None, Some(0.0), Some(0.5), Some(1.0), Some(f64::NAN)] {
                    assert_eq!(width(&activity_track(&theme, tick, fraction)), 8);
                }
            }
        }
    }

    #[test]
    fn effects_never_change_the_text_or_its_width() {
        for theme in [Theme::dark(), Theme::light(), Theme::mono()] {
            for sample in ["dial", "thinking", "日本語 ok", ""] {
                let g = gradient(&theme, sample, 0.3, Style::default());
                let s = shimmer(&theme, sample, 7, Style::default());
                for spans in [&g, &s] {
                    let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
                    assert_eq!(joined, sample);
                    assert_eq!(width(spans), text::width(sample));
                }
            }
        }
    }

    #[test]
    fn a_truecolor_brand_blends_across_the_text_and_mono_stays_solid() {
        let theme = Theme::dark();
        let spans = gradient(&theme, "dialdial", 0.0, Style::default());
        let first = spans.first().unwrap().style.fg;
        let last = spans.last().unwrap().style.fg;
        assert!(matches!(first, Some(Color::Rgb(..))));
        assert_ne!(first, last, "the gradient moves across the word");
        let mono = gradient(&Theme::mono(), "dial", 0.0, Style::default());
        assert_eq!(
            mono.len(),
            1,
            "no per-character colors without truecolor stops"
        );
    }

    #[test]
    fn the_shimmer_band_moves_with_the_tick() {
        let theme = Theme::dark();
        let base = theme.fg(Tok::Muted);
        let bright = |tick| {
            shimmer(&theme, "working on it", tick, base)
                .iter()
                .position(|s| s.style.add_modifier.contains(Modifier::BOLD))
        };
        assert_ne!(bright(3), bright(6), "the highlight advances");
        let rest = shimmer(&theme, "working on it", 13 + 6, base);
        assert!(
            rest.iter().all(|s| s.style == base),
            "the band rests between sweeps"
        );
    }

    #[test]
    fn brand_at_wraps_without_a_seam() {
        let theme = Theme::dark();
        assert_eq!(brand_at(&theme, 0.0), brand_at(&theme, 1.0));
        assert_eq!(brand_at(&theme, 0.25), brand_at(&theme, 1.25));
    }

    #[test]
    fn motion_needs_the_setting_truecolor_and_unicode_glyphs() {
        assert!(motion(true, &Theme::dark()));
        assert!(!motion(false, &Theme::dark()), "ui.motion = off");
        assert!(!motion(true, &Theme::mono()), "no truecolor stops");
        assert!(
            !motion(
                true,
                &Theme::dark().with_glyphs(crate::ui::theme::Glyphs::Ascii)
            ),
            "ASCII stays still"
        );
    }

    #[test]
    fn the_sweep_crosses_once_and_rests_on_the_static_gradient() {
        let theme = Theme::dark();
        let word = "▄▄█ ▀ ▄▄▄  █";
        let still = gradient(&theme, word, 0.1, Style::default());
        for done in [-0.5, 1.0, 3.0] {
            assert_eq!(sweep(&theme, word, 0.1, done, Style::default()), still);
        }
        let early = sweep(&theme, word, 0.1, 0.2, Style::default());
        let late = sweep(&theme, word, 0.1, 0.7, Style::default());
        assert_ne!(early, still, "the band is visible mid-sweep");
        assert_ne!(early, late, "the band moves");
        for spans in [&early, &late] {
            let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
            assert_eq!(joined, word, "only colors change");
        }
        let mono = Theme::mono();
        assert_eq!(
            sweep(&mono, word, 0.0, 0.5, Style::default()),
            gradient(&mono, word, 0.0, Style::default())
        );
    }

    #[test]
    fn a_reveal_unrolls_from_the_top_and_ends_untouched() {
        assert_eq!(reveal_rows(20, 0.0), 1);
        assert!(reveal_rows(20, 0.3) > reveal_rows(20, 0.1));
        assert_eq!(reveal_rows(20, 1.0), 20);
        assert_eq!(reveal_rows(0, 0.5), 0);
        let area = Rect::new(0, 0, 4, 4);
        let mut full = Buffer::filled(area, ratatui::buffer::Cell::new("x"));
        let untouched = full.clone();
        reveal(&mut full, area, 1.0);
        assert_eq!(full, untouched);
        reveal(&mut full, area, 0.0);
        assert_eq!(full[(0, 0)].symbol(), "x", "the top row shows at once");
        assert_eq!(full[(0, 3)].symbol(), " ", "lower rows wait");
        assert_eq!(progress(Duration::from_millis(70), REVEAL), 0.5);
        assert_eq!(progress(Duration::from_secs(9), Duration::ZERO), 1.0);
    }
}
