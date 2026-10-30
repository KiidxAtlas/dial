use super::*;
use crate::tools::Workspace;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{DynamicImage, ImageFormat, ImageReader};
use std::io::Cursor;

// Keep CPU, decoded allocation and model attachment bounded without silently changing coordinates.
const MAX_PIXELS: u64 = 16 * 1024 * 1024;
pub(super) struct Frame {
    pub image: DynamicImage,
    pub png: Vec<u8>,
    pub item: Evidence,
    pub page: Option<u32>,
}
pub(super) fn encode(image: &DynamicImage) -> Result<Vec<u8>> {
    let mut png = Cursor::new(Vec::new());
    image.write_to(&mut png, ImageFormat::Png)?;
    let png = png.into_inner();
    ensure!(
        png.len() as u64 <= crate::llm::images::MAX_IMAGE_BYTES,
        "canonical PNG exceeds 5 MiB; explicitly preprocess source before inspection (no implicit coordinate changes)"
    );
    Ok(png)
}
pub(super) fn decode(bytes: &[u8]) -> Result<DynamicImage> {
    let (w, h) = crate::llm::images::dimensions(bytes)?;
    ensure!(
        w > 0 && h > 0 && u64::from(w) * u64::from(h) <= MAX_PIXELS,
        "canonical image must contain 1..16M pixels"
    );
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    Ok(reader.decode()?)
}
pub(super) fn frame(source: &str, bytes: &[u8], page: Option<u32>) -> Result<Frame> {
    let image = decode(bytes)?;
    let png = encode(&image)?;
    let item = Evidence {
        source: source.into(), location: page.map_or(Location::Whole, |number| Location::Page { number }),
        method: Method::Render, producer: "dial/visual-canonical-png".into(), kind: EvidenceKind::Observation,
        summary: format!("Canonical {}x{} PNG pixel frame; selected page {page:?}", image.width(), image.height()),
        representation: Representation::of_bytes("image/png", &png),
        limitations: vec!["Canonical decoded pixels without EXIF rotation, color-profile interpretation or semantic analysis. Animated inputs use the decoder's first frame only. Pixel coordinates refer to this PNG, not original PDF points. Retention does not establish freshness.".into()],
        context: EvidenceContext::default(),
    };
    Ok(Frame {
        image,
        png,
        item,
        page,
    })
}
pub(super) fn load(ws: &Workspace, path: Option<&str>, reference: Option<&str>) -> Result<Frame> {
    match (path, reference) {
        (Some(path), None) => {
            let bytes = source::read(ws, path)?;
            let mut frame = frame(path, &bytes, None)?;
            frame.item.context.snapshot = Some(source::snapshot(&bytes)?);
            Ok(frame)
        }
        (None, Some(reference)) => {
            let store = ws
                .artifacts()
                .context("retained images require artifact store")?;
            let record = store.evidence_record(reference)?;
            ensure!(
                record
                    .evidence
                    .representation
                    .media_type
                    .starts_with("image/"),
                "reference must identify retained image evidence"
            );
            let bytes = store.evidence_bytes(&record, 16 * 1024 * 1024)?;
            let page = match record.evidence.location {
                Location::Page { number } => Some(number),
                _ => None,
            };
            let mut frame = frame(reference, &bytes, page)?;
            frame.item.context.parents.push(reference.into());
            Ok(frame)
        }
        _ => anyhow::bail!("provide exactly one image path or reference"),
    }
}
pub(super) fn attachment(png: &[u8]) -> Value {
    json!({"encoding":"base64","media_type":"image/png","data":STANDARD.encode(png)})
}
pub(super) fn compare(a: &DynamicImage, b: &DynamicImage, cancel: &AtomicBool) -> Result<Value> {
    let dimensions_match = a.width() == b.width() && a.height() == b.height();
    if !dimensions_match {
        return Ok(
            json!({"dimensions_match":false,"changed_pixels":null,"mean_absolute_rgba_delta":null,"limitation":"Different dimensions: no alignment/resampling assumed, pixel metrics unavailable. Geometry difference does not establish semantic change."}),
        );
    }
    let (a, b) = (a.to_rgba8(), b.to_rgba8());
    let mut changed = 0_u64;
    let mut sum = 0_u64;
    let mut min_x = a.width();
    let mut min_y = a.height();
    let mut max_x = 0;
    let mut max_y = 0;
    for (i, (p, q)) in a.pixels().zip(b.pixels()).enumerate() {
        if i % 65536 == 0 {
            ensure!(
                !cancel.load(Ordering::Acquire),
                "pixel comparison cancelled"
            );
        }
        if p != q {
            changed += 1;
            let x = i as u32 % a.width();
            let y = i as u32 / a.width();
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
        sum +=
            p.0.iter()
                .zip(q.0)
                .map(|(p, q)| u64::from(p.abs_diff(q)))
                .sum::<u64>();
    }
    let total = u64::from(a.width()) * u64::from(a.height());
    let bbox = (changed > 0)
        .then(|| json!({"x":min_x,"y":min_y,"width":max_x-min_x+1,"height":max_y-min_y+1}));
    Ok(
        json!({"dimensions_match":true,"total_pixels":total,"changed_pixels":changed,"changed_fraction":changed as f64 / total as f64,"mean_absolute_rgba_delta":sum as f64 / (total * 4) as f64,"changed_bounding_rectangle":bbox,"limitation":"Exact decoded RGBA channel deltas, no registration, tolerance, OCR or semantic interpretation. Alpha/color encoding differences may not correspond to visible semantic changes."}),
    )
}
