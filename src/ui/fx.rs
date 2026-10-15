//! Color effects shared by the welcome wordmark, the working status and turn markers.
//!
//! Everything here degrades instead of failing: a theme whose brand stops are not truecolor (an
//! indexed palette, `mono`, `NO_COLOR`) gets the start stop as one solid color, and ASCII glyph
//! sets are untouched because effects only change colors, never characters. Widths never change,
//! so callers can lay text out before styling it.
use super::theme::{Theme, Tok};
use ratatui::{
    style::{Color, Modifier, Style},
    text::Span,
};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::text;

    fn width(spans: &[Span]) -> usize {
        spans.iter().map(|s| text::width(&s.content)).sum()
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
}
