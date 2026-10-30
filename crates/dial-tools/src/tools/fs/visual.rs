//! Bounded canonical image frames, spatial annotations, pixel comparisons and coordinate OCR.
mod ocr;
mod raster;
#[cfg(test)]
mod tests;

use crate::config::types::Config;
use crate::runtime::evidence::{
    Evidence, EvidenceContext, EvidenceKind, Location, Method, Representation,
};
use crate::tools::ToolOutput;
use crate::tools::integrations::ext::{Tool, ToolContext};
use crate::tools::knowledge::evidence::{publish, source};
use crate::tools::knowledge::perception::{self, Spend};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Render one bounded copied PDF page for the legacy read surface after its owner authorizes execution.
pub(in crate::tools::fs) fn render_pdf_page(
    path: &str,
    bytes: &[u8],
    page: u32,
    cancel: &AtomicBool,
) -> Result<(Vec<u8>, crate::runtime::evidence::SourceSnapshot)> {
    ensure!((1..=10_000).contains(&page), "PDF page must be 1..10000");
    ensure!(bytes.starts_with(b"%PDF-"), "PDF source bytes required");
    let frame = ocr::render_pages(path, bytes, &[page], cancel)?
        .pop()
        .context("PDF render produced no frame")?;
    Ok((
        frame.png,
        frame
            .item
            .context
            .snapshot
            .context("PDF source snapshot missing")?,
    ))
}

/// Independent entry point for the built-in tool registry.
pub fn tools(_: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(Visual)]
}
struct Visual;
impl Tool for Visual {
    fn name(&self) -> &str {
        "visual"
    }
    fn schema(&self) -> Value {
        crate::tools::tool_schema(
            "visual",
            "Inspect a bounded image or retained image citation in an exact canonical PNG pixel frame; regions produces actual crop citations and optional caller/model-authored diagram edges, NOT automatic diagram understanding. The main image-capable model interprets attached pixels; no hidden model call. compare measures RGBA pixel changes only, not semantic changes. ocr runs installed tesseract with exact TSV word boxes/confidence (guesses, not truth). Explicit PDF pages (1..4 selected pages, max page 10000) are rendered with installed pdftoppm at <=2048px; page index and crop parent citations preserved. OCR/PDF processes require confirmation and controller authorization; no installation/network. Coordinates are integer pixels in returned canonical frame, top-left origin; no EXIF rotation/resizing of raster inputs. At most 16 regions/32 edges/500 OCR words per frame; source <=64 MiB, decoded frame <=16M pixels, canonical PNG <=5 MiB (otherwise explicitly preprocess). First frame only is attached; other citations can be inspected separately. Retention-limited untrusted evidence, never verification.",
            json!({
                "action":{"type":"string","enum":["inspect","regions","compare","ocr"]},
                "path":{"type":"string"},"reference":{"type":"string"},"other_path":{"type":"string"},"other_reference":{"type":"string"},
                "pages":{"type":"array","items":{"type":"integer","minimum":1,"maximum":10000},"minItems":1,"maxItems":4},
                "regions":{"type":"array","maxItems":16,"items":{"type":"object","properties":{"id":{"type":"string","maxLength":64},"x":{"type":"integer","minimum":0},"y":{"type":"integer","minimum":0},"width":{"type":"integer","minimum":1},"height":{"type":"integer","minimum":1},"label":{"type":"string","maxLength":512}},"required":["id","x","y","width","height"],"additionalProperties":false}},
                "edges":{"type":"array","maxItems":32,"items":{"type":"object","properties":{"from":{"type":"string"},"to":{"type":"string"},"relation":{"type":"string","maxLength":512}},"required":["from","to","relation"],"additionalProperties":false}},
                "language":{"type":"string","maxLength":64},"max_words":{"type":"integer","minimum":1,"maximum":500}
            }),
            &["action"],
        )
    }
    fn needs_approval(&self, args: &Value) -> bool {
        args["action"] == "ocr" || args.get("pages").is_some()
    }
    fn may_modify_workspace(&self, _: &Value) -> bool {
        false
    }
    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        execute(ctx, args, &AtomicBool::new(false))
    }
    fn call_streaming(
        &self,
        ctx: &ToolContext,
        args: &Value,
        _: &(dyn Fn(crate::tools::integrations::ext::ToolEvent) + Sync),
        cancel: &AtomicBool,
    ) -> ToolOutput {
        execute(ctx, args, cancel)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    Inspect,
    Regions,
    Compare,
    Ocr,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    action: Action,
    path: Option<String>,
    reference: Option<String>,
    other_path: Option<String>,
    other_reference: Option<String>,
    pages: Option<Vec<u32>>,
    #[serde(default)]
    regions: Vec<Region>,
    #[serde(default)]
    edges: Vec<Edge>,
    language: Option<String>,
    max_words: Option<usize>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Region {
    id: String,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    label: Option<String>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Edge {
    from: String,
    to: String,
    relation: String,
}

fn text_bound(text: &str, max: usize) -> bool {
    !text.trim().is_empty() && text.len() <= max && !text.chars().any(|c| c.is_control())
}
fn validate(args: &Args) -> Result<()> {
    ensure!(
        args.path.is_some() != args.reference.is_some(),
        "provide exactly one path or reference"
    );
    ensure!(
        args.regions.len() <= 16 && args.edges.len() <= 32,
        "at most 16 regions and 32 edges"
    );
    let mut ids = std::collections::HashSet::new();
    for r in &args.regions {
        ensure!(
            text_bound(&r.id, 64) && ids.insert(&r.id),
            "region ids must be unique, nonempty, <=64 bytes"
        );
        ensure!(
            r.width > 0
                && r.height > 0
                && r.x.checked_add(r.width).is_some()
                && r.y.checked_add(r.height).is_some(),
            "invalid or overflowing pixel rectangle"
        );
        ensure!(
            r.label.as_deref().is_none_or(|s| text_bound(s, 512)),
            "label must contain 1..512 bytes without controls"
        );
    }
    for e in &args.edges {
        ensure!(
            ids.contains(&e.from) && ids.contains(&e.to) && text_bound(&e.relation, 512),
            "edges must refer to provided regions and have a bounded relation"
        );
    }
    if matches!(args.action, Action::Compare) {
        ensure!(
            args.other_path.is_some() != args.other_reference.is_some(),
            "compare requires exactly one other_path or other_reference"
        );
        ensure!(
            args.pages.is_none() && args.regions.is_empty() && args.edges.is_empty(),
            "compare requires two raster frames; no PDF pages/regions"
        );
    } else {
        ensure!(
            args.other_path.is_none() && args.other_reference.is_none(),
            "other source is only valid for compare"
        );
    }
    if let Some(pages) = &args.pages {
        ensure!(
            args.path.is_some()
                && !pages.is_empty()
                && pages.len() <= 4
                && pages.iter().all(|p| (1..=10000).contains(p))
                && pages.windows(2).all(|p| p[0] < p[1]),
            "PDF requires 1..4 explicit increasing pages in 1..10000 and a path"
        );
    }
    ensure!(
        args.max_words.is_none_or(|n| (1..=500).contains(&n)),
        "max_words must be 1..500"
    );
    ensure!(
        args.language.as_deref().is_none_or(|s| !s.is_empty()
            && s.len() <= 64
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'+')),
        "language must be a bounded tesseract language name (no paths/options)"
    );
    ensure!(
        matches!(args.action, Action::Ocr) || (args.language.is_none() && args.max_words.is_none()),
        "language/max_words only apply to OCR"
    );
    Ok(())
}

/// Execute through the same producer boundary used by the active-perception controller.
pub(crate) fn execute(ctx: &ToolContext, args: &Value, cancel: &AtomicBool) -> ToolOutput {
    serde_json::from_value(args.clone())
        .map_err(anyhow::Error::from)
        .and_then(|args| run(ctx, args, cancel))
        .unwrap_or_else(|e| ToolOutput::err(format!("{e:#}")))
}

fn run(ctx: &ToolContext, args: Args, cancel: &AtomicBool) -> Result<ToolOutput> {
    validate(&args)?;
    ensure!(
        !cancel.load(Ordering::Acquire),
        "visual investigation cancelled"
    );
    let store = ctx
        .workspace
        .artifacts()
        .context("visual requires this project's artifact store")?;
    let pages = args.pages.as_ref().map_or(0, Vec::len);
    perception::reserve(
        ctx,
        Spend {
            images: if pages == 0 {
                if matches!(args.action, Action::Compare) {
                    2
                } else {
                    1
                }
            } else {
                0
            } + (args.regions.len() * pages.max(1)) as u64,
            pages: pages as u64,
            ..Default::default()
        },
    )?;
    if matches!(args.action, Action::Ocr) || args.pages.is_some() {
        super::audio::authorize(
            ctx,
            "visual",
            "Run installed local tesseract and/or pdftoppm outside the shell sandbox on a copied authorized source? No installation or network requested. OCR text is an uncertain machine guess.",
        )?;
    }
    let frames = if let Some(pages) = &args.pages {
        let path = args.path.as_deref().context("PDF path required")?;
        let bytes = source::read(ctx.workspace, path)?;
        ensure!(
            bytes.starts_with(b"%PDF-"),
            "pages requires PDF source bytes"
        );
        ocr::render_pages(path, &bytes, pages, cancel)?
    } else {
        vec![raster::load(
            ctx.workspace,
            args.path.as_deref(),
            args.reference.as_deref(),
        )?]
    };
    let mut results = Vec::new();
    let mut parents = Vec::new();
    let mut first_png = None;
    for frame in frames {
        ensure!(
            !cancel.load(Ordering::Acquire),
            "visual investigation cancelled"
        );
        for region in &args.regions {
            region.check(frame.image.width(), frame.image.height())?;
        }
        let citation = store.retain_evidence(&frame.item, &frame.png)?;
        parents.push(citation.clone());
        if first_png.is_none() {
            first_png = Some(frame.png.clone());
        }
        let mut data = json!({"citation":citation,"width":frame.image.width(),"height":frame.image.height(),"page":frame.page,"coordinate_frame":"canonical PNG integer pixels; origin top-left, x right, y down; no EXIF transform","regions":[],"edges":[],"interpretation":"pixels supplied to main image-capable model; no automatic semantic interpretation"});
        let mut regions = Vec::new();
        for region in &args.regions {
            let png = raster::encode(&frame.image.crop_imm(
                region.x,
                region.y,
                region.width,
                region.height,
            ))?;
            let mut item = frame.item.clone();
            item.source = citation.clone();
            item.context = EvidenceContext {
                parents: vec![citation.clone()],
                ..Default::default()
            };
            item.location = region.location(frame.image.width(), frame.image.height());
            item.representation = Representation::of_bytes("image/png", &png);
            item.summary = format!(
                "Measured pixel crop ({},{},{},{}) of canonical frame; page {:?}",
                region.x, region.y, region.width, region.height, frame.page
            );
            item.method = Method::Render;
            item.producer = "dial/visual-crop".into();
            let crop = store.retain_evidence(&item, &png)?;
            regions.push(json!({"id":region.id,"rectangle":region,"citation":crop,"parent":citation,"page":frame.page,"label_attribution":"caller/model interpretation, not measured semantics"}));
        }
        data["regions"] = json!(regions);
        data["edges"] = json!(args.edges.iter().map(|e| json!({"from":e.from,"to":e.to,"relation":e.relation,"attribution":"caller/model interpretation; NOT inferred by this tool","support":regions.iter().filter(|r| r["id"] == e.from || r["id"] == e.to).map(|r| r["citation"].clone()).collect::<Vec<_>>()})).collect::<Vec<_>>());
        if matches!(args.action, Action::Ocr) {
            data["ocr"] = ocr::recognize(
                &frame.png,
                frame.image.width(),
                frame.image.height(),
                args.language.as_deref().unwrap_or("eng"),
                args.max_words.unwrap_or(200),
                cancel,
            )?;
        }
        if matches!(args.action, Action::Compare) {
            let other = raster::load(
                ctx.workspace,
                args.other_path.as_deref(),
                args.other_reference.as_deref(),
            )?;
            let other_citation = store.retain_evidence(&other.item, &other.png)?;
            parents.push(other_citation.clone());
            data["comparison"] = raster::compare(&frame.image, &other.image, cancel)?;
            data["other"] = json!({"citation":other_citation,"width":other.image.width(),"height":other.image.height()});
        }
        results.push(data);
    }
    let data = json!({"frames":results,"limitations":"OCR guesses and caller/model labels/relations are not measurements or verification. Pixel deltas are not semantic comparison. Only explicitly selected pages/regions inspected. Reopen citations for additional model-visible images."});
    let text = serde_json::to_string_pretty(&data)?;
    ensure!(
        text.len() <= 512 * 1024,
        "visual result exceeds 512 KiB; reduce regions/pages/words"
    );
    let interpreted = !args.edges.is_empty() || args.regions.iter().any(|r| r.label.is_some());
    let item = Evidence {
        source: parents[0].clone(), location: Location::Whole,
        method: if interpreted { Method::ModelInterpretation } else if matches!(args.action, Action::Ocr) { Method::Ocr } else { Method::Calculation },
        producer: "dial/visual-v1".into(), kind: if interpreted || matches!(args.action, Action::Ocr) { EvidenceKind::Inference } else { EvidenceKind::Observation },
        summary: "Bounded canonical frame investigation; interpretation explicitly attributed".into(),
        representation: Representation::of_bytes("application/json", text.as_bytes()),
        limitations: vec!["Pixel geometry is measured; OCR, labels and relations remain fallible interpretations. No semantic verification or source freshness claim.".into()],
        context: EvidenceContext { parents, ..Default::default() },
    };
    let mut out = ToolOutput::ok(text.clone());
    out.data = Some(data);
    if let Some(png) = first_png {
        out.data.as_mut().context("visual data missing")?["artifact"] = raster::attachment(&png);
    }
    publish(ctx.workspace, out, item, text.as_bytes())
}

impl Region {
    fn check(&self, width: u32, height: u32) -> Result<()> {
        ensure!(
            self.width > 0
                && self.height > 0
                && self.x.checked_add(self.width).is_some_and(|n| n <= width)
                && self.y.checked_add(self.height).is_some_and(|n| n <= height),
            "region {} lies outside canonical {width}x{height} frame",
            self.id
        );
        Ok(())
    }
    fn location(&self, width: u32, height: u32) -> Location {
        // Evidence's shared location is normalized; exact pixel rectangle stays in the manifest.
        let scale = |v: u32, total: u32| (u64::from(v) * 1_000_000 / u64::from(total)) as u32;
        let x = scale(self.x, width);
        let y = scale(self.y, height);
        Location::Region {
            x,
            y,
            width: scale(self.x + self.width, width).saturating_sub(x).max(1),
            height: scale(self.y + self.height, height).saturating_sub(y).max(1),
        }
    }
}
