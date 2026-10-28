use super::*;

fn probe(duration: Option<f64>, fps: Option<f64>) -> Probe {
    Probe {
        duration,
        fps,
        width: 1920,
        height: 1080,
        video_codec: "h264".to_string(),
        audio_codec: None,
    }
}

#[test]
fn split_keeps_clock_colons_in_video_selectors() {
    assert_eq!(split("clip.mp4:01:23"), ("clip.mp4", "01:23"));
    assert_eq!(split("a.pdf:page=2:img"), ("a.pdf", "page=2:img"));
    assert_eq!(split("plain"), ("plain", ""));
}

#[test]
fn pdf_page_selector_requires_image_suffix_and_positive_page() {
    assert_eq!(pdf_page("page=3:img"), Some(3));
    assert_eq!(pdf_page("page=0:img"), None);
    assert_eq!(pdf_page("page=3"), None);
}

#[test]
fn unit_durations_sum_hours_minutes_and_seconds() {
    assert_eq!(parse_units("1h5m42s"), Some(3942.0));
    assert_eq!(parse_units("90s"), Some(90.0));
    assert_eq!(parse_units("12.5s"), Some(12.5));
    assert_eq!(parse_units("5"), None);
    assert_eq!(parse_units("1h5"), None);
}

#[test]
fn clock_selectors_accept_mm_ss_and_hh_mm_ss_only() {
    assert_eq!(parse_clock("01:23"), Some(83.0));
    assert_eq!(parse_clock("1:02:03"), Some(3723.0));
    assert_eq!(parse_clock("1:2:3:4"), None);
}

#[test]
fn frame_numbers_map_through_frame_rate_and_respect_duration() {
    let video = probe(Some(10.0), Some(30.0));
    assert_eq!(locate("30", &video).unwrap(), 1.0);
    assert!(locate("300", &video).is_err());
}

#[test]
fn positions_beyond_duration_are_rejected() {
    let video = probe(Some(10.0), Some(30.0));
    assert!(locate("11s", &video).is_err());
    assert!(locate("bogus", &video).is_err());
}

#[test]
fn frame_rate_rationals_parse_and_reject_zero_denominators() {
    assert!((parse_rate("30000/1001").unwrap() - 29.97).abs() < 0.01);
    assert_eq!(parse_rate("0/0"), None);
}

#[test]
fn svg_rasterizes_to_a_png_at_the_minimum_legible_edge() {
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="20"><rect width="10" height="20" fill="red"/></svg>"#;
    let png = rasterize_svg(svg).unwrap();
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert_eq!(images::dimensions(&png).unwrap(), (512, 1024));
}

#[test]
fn invalid_svg_is_rejected() {
    assert!(rasterize_svg(b"not an svg").is_err());
}
