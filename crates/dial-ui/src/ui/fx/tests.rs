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
