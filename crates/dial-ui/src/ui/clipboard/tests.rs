use super::*;
use image::{ImageBuffer, Rgb, Rgba};
use std::path::PathBuf;

fn gradient(width: u32, height: u32) -> DynamicImage {
    DynamicImage::ImageRgba8(ImageBuffer::from_fn(width, height, |x, y| {
        Rgba([(x % 256) as u8, (y % 256) as u8, ((x ^ y) % 256) as u8, 255])
    }))
}

/// Deterministic pseudo-random pixels; they compress poorly, so PNG size stays large.
fn noise(width: u32, height: u32) -> DynamicImage {
    let mut state: u32 = 0x9e37_79b9;
    DynamicImage::ImageRgb8(ImageBuffer::from_fn(width, height, |_, _| {
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state & 0xff) as u8
        };
        Rgb([next(), next(), next()])
    }))
}

fn encode(image: &DynamicImage, format: ImageFormat) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    image.write_to(&mut out, format).expect("encode test image");
    out.into_inner()
}

fn is_png(bytes: &[u8]) -> bool {
    image::guess_format(bytes).is_ok_and(|format| format == ImageFormat::Png)
}

#[test]
fn png_within_limit_passes_through_unchanged() {
    let png = encode(&gradient(16, 8), ImageFormat::Png);
    let normalized = normalize_png(&png).expect("png accepted");
    assert_eq!(normalized, png);
}

#[test]
fn jpeg_gif_and_webp_are_reencoded_as_png_with_same_dimensions() {
    let cases = [
        (encode(&noise(20, 12), ImageFormat::Jpeg), ImageFormat::Jpeg),
        (
            encode(&gradient(20, 12), ImageFormat::Gif),
            ImageFormat::Gif,
        ),
        (
            encode(&gradient(20, 12), ImageFormat::WebP),
            ImageFormat::WebP,
        ),
    ];
    for (bytes, source) in cases {
        let normalized = normalize_png(&bytes)
            .unwrap_or_else(|error| panic!("{source:?} should normalize: {error:#}"));
        assert!(is_png(&normalized), "{source:?} output must be PNG");
        let decoded = image::load_from_memory_with_format(&normalized, ImageFormat::Png)
            .expect("normalized output decodes");
        assert_eq!((decoded.width(), decoded.height()), (20, 12));
    }
}

#[test]
fn oversized_png_is_downscaled_to_fit_the_limit() {
    let png = encode(&noise(240, 240), ImageFormat::Png);
    let limit = 40_000;
    assert!(png.len() as u64 > limit, "fixture must exceed the limit");
    let fitted = normalize_within(&png, limit).expect("downscaled to fit");
    assert!(fitted.len() as u64 <= limit);
    assert!(is_png(&fitted));
    let decoded = image::load_from_memory_with_format(&fitted, ImageFormat::Png).unwrap();
    assert!(decoded.width() < 240 && decoded.height() < 240);
}

#[test]
fn unsupported_or_corrupt_data_is_rejected_with_a_reason() {
    let text = normalize_png(b"just some clipboard text").unwrap_err();
    assert!(format!("{text:#}").contains("not a PNG, JPEG, GIF or WebP"));

    let mut bmp = b"BM".to_vec();
    bmp.extend_from_slice(&[0; 64]);
    let error = normalize_png(&bmp).unwrap_err();
    assert!(format!("{error:#}").contains("Bmp"));

    let mut truncated = encode(&gradient(64, 64), ImageFormat::Png);
    truncated.truncate(truncated.len() / 2);
    assert!(normalize_png(&truncated).is_err());
}

#[test]
fn empty_and_oversized_payloads_are_rejected_before_decoding() {
    assert!(normalize_png(&[]).is_err());
    let oversized = vec![0_u8; (MAX_CLIPBOARD_BYTES + 1) as usize];
    let error = normalize_png(&oversized).unwrap_err();
    assert!(format!("{error:#}").contains("limit is"));
}

#[test]
fn macos_selects_pasteboard_then_finder_files() {
    let env = Environment {
        os: Os::MacOs,
        wsl: false,
        termux: false,
        wayland: false,
        x11: false,
    };
    assert_eq!(
        readers(env),
        vec![Reader::MacPasteboard, Reader::MacFileUrls]
    );
}

#[test]
fn windows_and_other_platforms_select_powershell_or_nothing() {
    let base = Environment {
        os: Os::Windows,
        wsl: false,
        termux: false,
        wayland: false,
        x11: false,
    };
    assert_eq!(readers(base), vec![Reader::PowerShell]);
    let other = Environment {
        os: Os::Other,
        ..base
    };
    assert!(readers(other).is_empty());
}

#[test]
fn linux_selects_by_display_server() {
    let linux = Environment {
        os: Os::Linux,
        wsl: false,
        termux: false,
        wayland: false,
        x11: false,
    };
    assert_eq!(
        readers(Environment {
            wayland: true,
            ..linux
        }),
        vec![Reader::WlPaste]
    );
    assert_eq!(
        readers(Environment { x11: true, ..linux }),
        vec![Reader::Xclip]
    );
    assert_eq!(
        readers(Environment {
            wayland: true,
            x11: true,
            ..linux
        }),
        vec![Reader::WlPaste, Reader::Xclip]
    );
    // With no display variable exported, both tools are still tried.
    assert_eq!(readers(linux), vec![Reader::WlPaste, Reader::Xclip]);
}

#[test]
fn wsl_prefers_the_windows_clipboard_before_wslg_bridges() {
    let wsl = Environment {
        os: Os::Linux,
        wsl: true,
        termux: false,
        wayland: true,
        x11: false,
    };
    assert_eq!(readers(wsl), vec![Reader::PowerShell, Reader::WlPaste]);
    let headless = Environment {
        wayland: false,
        ..wsl
    };
    assert_eq!(
        readers(headless),
        vec![Reader::PowerShell, Reader::WlPaste, Reader::Xclip]
    );
}

#[test]
fn termux_has_no_image_api_unless_an_x_server_is_present() {
    let termux = Environment {
        os: Os::Linux,
        wsl: false,
        termux: true,
        wayland: false,
        x11: false,
    };
    assert_eq!(readers(termux), vec![Reader::Termux]);
    assert_eq!(
        readers(Environment {
            x11: true,
            ..termux
        }),
        vec![Reader::Xclip, Reader::Termux]
    );
}

#[test]
fn first_image_stops_at_the_first_reader_with_an_image() {
    let mut calls = Vec::new();
    let image = first_image(
        &[Reader::WlPaste, Reader::Xclip, Reader::PowerShell],
        |reader| {
            calls.push(reader);
            Ok(match reader {
                Reader::WlPaste => None,
                Reader::Xclip => Some(vec![1, 2, 3]),
                _ => panic!("readers after a hit must not run"),
            })
        },
    )
    .expect("image found");
    assert_eq!(image, vec![1, 2, 3]);
    assert_eq!(calls, vec![Reader::WlPaste, Reader::Xclip]);
}

#[test]
fn first_image_error_names_every_reader_and_its_outcome() {
    let error = first_image(&[Reader::WlPaste, Reader::Xclip], |reader| match reader {
        Reader::WlPaste => Ok(None),
        _ => Err(anyhow::anyhow!("xclip is not installed")),
    })
    .unwrap_err();
    let message = format!("{error:#}");
    assert!(message.starts_with("no clipboard image found"));
    assert!(message.contains("wl-paste: no image"));
    assert!(message.contains("xclip: xclip is not installed"));
}

#[test]
fn first_image_reports_unsupported_platform_when_no_readers_apply() {
    let error = first_image(&[], |_| unreachable!()).unwrap_err();
    assert!(format!("{error:#}").contains("not supported on this platform"));
}

#[test]
fn file_urls_resolve_to_local_paths_and_ignore_other_schemes() {
    let urls = vec![
        "file:///tmp/a%20b.png".to_owned(),
        "https://example.com/cat.png".to_owned(),
        "not a url".to_owned(),
    ];
    assert_eq!(file_url_paths(&urls), vec![PathBuf::from("/tmp/a b.png")]);
}

#[test]
fn image_file_returns_images_and_skips_other_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let image_path = dir.path().join("shot.png");
    let png = encode(&gradient(4, 4), ImageFormat::Png);
    std::fs::write(&image_path, &png).unwrap();
    assert_eq!(image_file(&image_path).unwrap(), Some(png));

    let text_path = dir.path().join("notes.txt");
    std::fs::write(&text_path, b"plain text").unwrap();
    assert_eq!(image_file(&text_path).unwrap(), None);
    assert_eq!(image_file(dir.path()).unwrap(), None);
}

#[test]
fn powershell_output_is_base64_png_or_absent() {
    assert_eq!(parse_powershell_png(b"").unwrap(), None);
    assert_eq!(parse_powershell_png(b"  \r\n").unwrap(), None);
    assert_eq!(
        parse_powershell_png(b"aGVsbG8=\r\n").unwrap(),
        Some(b"hello".to_vec())
    );
    assert!(parse_powershell_png(b"%%% not base64").is_err());
}

#[test]
fn powershell_script_is_sent_as_utf16le() {
    let encoded = script_utf16le("ab");
    assert_eq!(encoded, vec![b'a', 0, b'b', 0]);
}

#[cfg(unix)]
#[test]
fn capture_reports_missing_programs_as_absent() {
    let captured = capture(
        "dial-definitely-missing-clipboard-tool",
        &[],
        1024,
        Duration::from_secs(1),
    )
    .expect("missing program is not an error");
    assert!(captured.is_none());
}

#[cfg(unix)]
#[test]
fn capture_returns_output_and_exit_status() {
    let captured = capture(
        "sh",
        &["-c", "printf hi; exit 3"],
        1024,
        Duration::from_secs(5),
    )
    .unwrap()
    .expect("sh is installed");
    assert!(!captured.success);
    assert_eq!(captured.stdout, b"hi");
}

#[cfg(unix)]
#[test]
fn capture_enforces_the_output_limit() {
    let error = capture(
        "sh",
        &["-c", "head -c 100 /dev/zero"],
        10,
        Duration::from_secs(5),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("more data than the clipboard limit"));
}

#[cfg(unix)]
#[test]
fn capture_times_out_and_kills_slow_programs() {
    let error = capture("sh", &["-c", "sleep 30"], 1024, Duration::from_millis(200)).unwrap_err();
    assert!(format!("{error:#}").contains("timed out"));
}
