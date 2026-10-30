use super::*;
use crate::voice::process::{self, CommandSpec};
use std::{path::Path, time::Duration};

pub(super) fn render_pages(
    path: &str,
    bytes: &[u8],
    pages: &[u32],
    cancel: &AtomicBool,
) -> Result<Vec<raster::Frame>> {
    let converter = process::which(Path::new("pdftoppm")).context(
        "pdftoppm is not installed; PDF rendering unavailable (no automatic installation)",
    )?;
    let dir = tempfile::tempdir()?;
    let input = dir.path().join("source.pdf");
    std::fs::write(&input, bytes)?;
    let snapshot = source::snapshot(bytes)?;
    let mut frames = Vec::new();
    for page in pages {
        // A single explicit page per invocation; no unbounded scan or output directory fanout.
        let spec = CommandSpec::new(&converter)
            .arg("-png")
            .arg("-singlefile")
            .arg("-scale-to")
            .arg("2048")
            .arg("-f")
            .arg(page.to_string())
            .arg("-l")
            .arg(page.to_string())
            .arg(&input);
        let output = process::run(&spec, None, cancel, Duration::from_secs(30))?;
        let mut frame = raster::frame(path, &output.stdout, Some(*page))?;
        frame.item.context.snapshot = Some(snapshot.clone());
        frame.item.producer = "dial/visual-pdftoppm".into();
        frame.item.limitations.push("Only explicitly selected page rendered, longest edge <=2048 pixels; all other pages uninspected. Page pixel frame is not original PDF coordinates.".into());
        frames.push(frame);
    }
    Ok(frames)
}

pub(super) fn recognize(
    png: &[u8],
    width: u32,
    height: u32,
    language: &str,
    max_words: usize,
    cancel: &AtomicBool,
) -> Result<Value> {
    let tesseract = process::which(Path::new("tesseract")).context(
        "tesseract is not installed; coordinate OCR unavailable (no automatic installation)",
    )?;
    let spec = CommandSpec::new(tesseract)
        .arg("stdin")
        .arg("stdout")
        .arg("-l")
        .arg(language)
        .arg("tsv");
    let output = process::run(&spec, Some(png), cancel, Duration::from_secs(30))?;
    ensure!(
        output.stdout.len() <= 8 * 1024 * 1024,
        "OCR TSV exceeds 8 MiB; select a smaller crop"
    );
    parse_tsv(
        std::str::from_utf8(&output.stdout).context("OCR TSV is not UTF-8")?,
        width,
        height,
        max_words,
    )
}

pub(super) fn parse_tsv(tsv: &str, width: u32, height: u32, max_words: usize) -> Result<Value> {
    ensure!((1..=500).contains(&max_words), "max_words must be 1..500");
    let mut lines = tsv.lines();
    ensure!(
        lines.next()
            == Some(
                "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext"
            ),
        "unexpected tesseract TSV header"
    );
    let mut words = Vec::new();
    let mut total = 0;
    let mut text_bytes = 0;
    let mut truncated = false;
    for line in lines {
        let fields: Vec<_> = line.splitn(12, '\t').collect();
        ensure!(fields.len() == 12, "malformed tesseract TSV row");
        if fields[0] != "5" || fields[11].trim().is_empty() {
            continue;
        }
        let number = |i: usize| {
            fields[i]
                .parse::<u32>()
                .context("invalid OCR coordinate/hierarchy number")
        };
        let (x, y, w, h) = (number(6)?, number(7)?, number(8)?, number(9)?);
        ensure!(
            w > 0
                && h > 0
                && x.checked_add(w).is_some_and(|n| n <= width)
                && y.checked_add(h).is_some_and(|n| n <= height),
            "OCR box outside canonical {width}x{height} image"
        );
        let confidence: f64 = fields[10].parse().context("invalid OCR confidence")?;
        ensure!(
            confidence.is_finite() && (0.0..=100.0).contains(&confidence),
            "OCR word confidence must be finite 0..100"
        );
        ensure!(fields[11].len() <= 4096, "OCR word exceeds 4096 bytes");
        total += 1;
        if words.len() >= max_words || text_bytes + fields[11].len() > 32 * 1024 {
            truncated = true;
            continue;
        }
        text_bytes += fields[11].len();
        words.push(json!({"text":fields[11],"x":x,"y":y,"width":w,"height":h,"confidence":confidence,"block":number(2)?,"paragraph":number(3)?,"line":number(4)?,"word":number(5)?}));
    }
    Ok(
        json!({"words":words,"returned_words":words.len(),"total_detected_words":total,"truncated":truncated,"source_width":width,"source_height":height,"producer":"installed tesseract TSV","attribution":"machine OCR guesses; confidence is tesseract's uncalibrated score, not probability or verification","coordinate_frame":"exact integer boxes in canonical PNG, no inferred reading/diagram relationships"}),
    )
}
