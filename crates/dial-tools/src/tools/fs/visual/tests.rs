use super::*;
use crate::storage::artifact::ArtifactStore;
use crate::tools::{Workspace, testkit};
use image::{DynamicImage, Rgba, RgbaImage};
use std::fs;

fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Workspace) {
    let (dir, ws) = testkit::workspace();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(ArtifactStore::new(state.path()).unwrap());
    (dir, state, ws)
}
fn invoke(ws: &Workspace, value: Value) -> Result<ToolOutput> {
    let config = Config::default();
    let ctx = ToolContext {
        workspace: ws,
        config: &config,
        confirm: None,
        mutation_authorizer: None,
    };
    run(
        &ctx,
        serde_json::from_value(value)?,
        &AtomicBool::new(false),
    )
}
fn png() -> Vec<u8> {
    let image = RgbaImage::from_fn(8, 6, |x, y| Rgba([x as u8 * 20, y as u8 * 20, 30, 255]));
    raster::encode(&DynamicImage::ImageRgba8(image)).unwrap()
}
#[test]
fn arguments_reject_ambiguous_sources_unbounded_selections_and_ungrounded_edges() {
    let valid = json!({"action":"inspect","path":"a.png"});
    assert!(validate(&serde_json::from_value(valid).unwrap()).is_ok());
    for value in [
        json!({"action":"inspect"}),
        json!({"action":"inspect","path":"a","reference":"b"}),
        json!({"action":"inspect","path":"a","pages":[]}),
        json!({"action":"inspect","path":"a","pages":[1,2,3,4,5]}),
        json!({"action":"inspect","path":"a","pages":[0]}),
        json!({"action":"inspect","path":"a","pages":[2,2]}),
        json!({"action":"inspect","path":"a","pages":[10001]}),
        json!({"action":"compare","path":"a"}),
        json!({"action":"ocr","path":"a","max_words":501}),
        json!({"action":"ocr","path":"a","language":"../../evil"}),
        json!({"action":"inspect","path":"a","edges":[{"from":"missing","to":"missing","relation":"flows"}]}),
        json!({"action":"regions","path":"a","regions":[{"id":"r","x":4294967295_u32,"y":0,"width":2,"height":1}]}),
    ] {
        let args: Args = serde_json::from_value(value.clone()).unwrap();
        assert!(validate(&args).is_err(), "accepted {value}");
    }
    assert!(
        serde_json::from_value::<Args>(json!({"action":"inspect","path":"a","unknown":true}))
            .is_err()
    );
}
#[test]
fn canonical_pixels_crops_and_caller_edges_have_reachable_parent_citations() {
    let (dir, _state, ws) = fixture();
    fs::write(dir.path().join("a.png"), png()).unwrap();
    let out = invoke(
        &ws,
        json!({"action":"regions","path":"a.png","regions":[
        {"id":"left","x":1,"y":2,"width":2,"height":3,"label":"source"},
        {"id":"right","x":5,"y":2,"width":2,"height":3,"label":"sink"}
    ],"edges":[{"from":"left","to":"right","relation":"caller says flows to"}]}),
    )
    .unwrap();
    let data = out.data.as_ref().unwrap();
    let frame = &data["frames"][0];
    assert_eq!(
        (frame["width"].as_u64(), frame["height"].as_u64()),
        (Some(8), Some(6))
    );
    let store = ws.artifacts().unwrap();
    let parent = frame["citation"].as_str().unwrap();
    let record = store.evidence_record(parent).unwrap();
    let canonical = raster::decode(&store.evidence_bytes(&record, 1024 * 1024).unwrap())
        .unwrap()
        .to_rgba8();
    assert_eq!(canonical.get_pixel(1, 2), &Rgba([20, 40, 30, 255]));
    let crop_ref = frame["regions"][0]["citation"].as_str().unwrap();
    let crop_record = store.evidence_record(crop_ref).unwrap();
    assert_eq!(crop_record.evidence.context.parents, vec![parent]);
    let crop = raster::decode(&store.evidence_bytes(&crop_record, 1024 * 1024).unwrap())
        .unwrap()
        .to_rgba8();
    assert_eq!(crop.dimensions(), (2, 3));
    assert_eq!(crop.get_pixel(0, 0), canonical.get_pixel(1, 2));
    assert_eq!(frame["edges"][0]["support"].as_array().unwrap().len(), 2);
    assert!(
        frame["edges"][0]["attribution"]
            .as_str()
            .unwrap()
            .contains("NOT inferred")
    );
    assert_eq!(
        out.evidence().unwrap().unwrap().items[0].kind,
        EvidenceKind::Inference
    );
    fs::remove_file(dir.path().join("a.png")).unwrap();
    let reopened = invoke(&ws, json!({"action":"inspect","reference":parent})).unwrap();
    assert_eq!(reopened.data.as_ref().unwrap()["frames"][0]["width"], 8);
    assert!(invoke(&ws, json!({"action":"regions","reference":parent,"regions":[{"id":"bad","x":7,"y":0,"width":2,"height":1}]})).is_err());
}
#[test]
fn pixel_comparison_reports_real_geometry_not_semantic_change() {
    let a = DynamicImage::ImageRgba8(RgbaImage::from_pixel(4, 3, Rgba([0, 0, 0, 255])));
    let mut b = a.to_rgba8();
    b.put_pixel(2, 1, Rgba([100, 0, 0, 255]));
    let cancel = AtomicBool::new(false);
    let delta = raster::compare(&a, &DynamicImage::ImageRgba8(b), &cancel).unwrap();
    assert_eq!(delta["changed_pixels"], 1);
    assert_eq!(
        delta["changed_bounding_rectangle"],
        json!({"x":2,"y":1,"width":1,"height":1})
    );
    assert!((delta["mean_absolute_rgba_delta"].as_f64().unwrap() - 100.0 / 48.0).abs() < 1e-10);
    assert!(
        delta["limitation"]
            .as_str()
            .unwrap()
            .contains("no registration")
    );
    assert_eq!(
        raster::compare(&a, &a, &cancel).unwrap()["changed_pixels"],
        0
    );
    let different = DynamicImage::new_rgba8(5, 3);
    let delta = raster::compare(&a, &different, &cancel).unwrap();
    assert_eq!(delta["dimensions_match"], false);
    assert!(delta["changed_pixels"].is_null());
    assert!(raster::compare(&a, &a, &AtomicBool::new(true)).is_err());
}
const TSV_HEADER: &str = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n";
#[test]
fn tsv_retains_exact_boxes_confidence_and_discloses_output_truncation() {
    let tsv = format!(
        "{TSV_HEADER}5\t1\t2\t3\t4\t1\t10\t20\t30\t15\t87.25\tHello\n5\t1\t2\t3\t4\t2\t45\t20\t20\t15\t62.5\tworld\n"
    );
    let data = ocr::parse_tsv(&tsv, 100, 80, 1).unwrap();
    assert_eq!(
        data["words"][0],
        json!({"text":"Hello","x":10,"y":20,"width":30,"height":15,"confidence":87.25,"block":2,"paragraph":3,"line":4,"word":1})
    );
    assert_eq!(data["truncated"], true);
    assert_eq!(data["total_detected_words"], 2);
    for bad in [
        tsv.replace("87.25", "NaN"),
        tsv.replace("87.25", "101"),
        tsv.replace("\t10\t20\t30\t15", "\t99\t20\t30\t15"),
        "bad header\n".into(),
    ] {
        assert!(ocr::parse_tsv(&bad, 100, 80, 20).is_err());
    }
}
#[test]
fn external_execution_requires_both_approval_boundaries_and_cancellation_is_early() {
    let (_dir, _state, ws) = fixture();
    let config = Config::default();
    let value = json!({"action":"ocr","path":"unavailable.png"});
    let ctx = ToolContext {
        workspace: &ws,
        config: &config,
        confirm: None,
        mutation_authorizer: None,
    };
    assert!(
        execute(&ctx, &value, &AtomicBool::new(false))
            .text
            .contains("confirmation")
    );
    let ctx = ToolContext {
        confirm: Some(&|_| true),
        ..ctx
    };
    assert!(
        execute(&ctx, &value, &AtomicBool::new(false))
            .text
            .contains("controller")
    );
    assert!(
        execute(&ctx, &value, &AtomicBool::new(true))
            .text
            .contains("cancelled")
    );
    assert!(Visual.needs_approval(&value));
    assert!(Visual.needs_approval(&json!({"action":"inspect","path":"a.pdf","pages":[1]})));
    assert!(!Visual.needs_approval(&json!({"action":"inspect","path":"a.png"})));
}

// Valid two-page PDF with large text; no external generator or downloaded fixture required.
fn pdf() -> Vec<u8> {
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".to_owned(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 180] /Resources << /Font << /F1 5 0 R >> >> /Contents 6 0 R >>".to_owned(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 180] /Resources << /Font << /F1 5 0 R >> >> /Contents 7 0 R >>".to_owned(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
        "BT /F1 48 Tf 35 80 Td (FIRST PAGE) Tj ET".to_owned(),
        "BT /F1 48 Tf 35 80 Td (DIAL SECOND) Tj ET".to_owned(),
    ];
    let mut pdf = "%PDF-1.4\n".to_owned();
    let mut offsets = vec![0];
    for (i, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        let object = if i >= 5 {
            format!(
                "<< /Length {} >>\nstream\n{object}\nendstream",
                object.len()
            )
        } else {
            object.clone()
        };
        pdf.push_str(&format!("{} 0 obj\n{object}\nendobj\n", i + 1));
    }
    let xref = pdf.len();
    pdf.push_str(&format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len()));
    for offset in offsets.iter().skip(1) {
        pdf.push_str(&format!("{offset:010} 00000 n \n"));
    }
    pdf.push_str(&format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        offsets.len()
    ));
    pdf.into_bytes()
}
#[test]
fn installed_pdf_and_ocr_tier_preserves_page_crop_provenance_and_real_word_boxes() {
    use crate::voice::process;
    use std::path::Path;
    if process::which(Path::new("pdftoppm")).is_none() {
        eprintln!("SKIP optional PDF render tier: pdftoppm unavailable");
        return;
    }
    let (dir, _state, ws) = fixture();
    fs::write(dir.path().join("a.pdf"), pdf()).unwrap();
    let config = Config::default();
    let ctx = ToolContext {
        workspace: &ws,
        config: &config,
        confirm: Some(&|_| true),
        mutation_authorizer: Some(&|_, _, _| Ok(())),
    };
    let out = run(&ctx,serde_json::from_value(json!({"action":"inspect","path":"a.pdf","pages":[2],"regions":[{"id":"text","x":0,"y":0,"width":1000,"height":600}]})).unwrap(),&AtomicBool::new(false)).unwrap();
    let data = out.data.as_ref().unwrap();
    let frame = &data["frames"][0];
    assert_eq!(frame["page"], 2);
    assert_eq!(frame["width"], 2048);
    assert_eq!(frame["regions"][0]["page"], 2);
    let store = ws.artifacts().unwrap();
    let parent = frame["citation"].as_str().unwrap();
    let record = store.evidence_record(parent).unwrap();
    assert_eq!(record.evidence.location, Location::Page { number: 2 });
    let crop = store
        .evidence_record(frame["regions"][0]["citation"].as_str().unwrap())
        .unwrap();
    assert_eq!(crop.evidence.context.parents, vec![parent]);
    eprintln!(
        "LIVE pdftoppm: rendered only requested page 2, canonical {}x{}, retained crop with page parent",
        frame["width"], frame["height"]
    );
    if process::which(Path::new("tesseract")).is_none() {
        eprintln!("SKIP optional OCR tier: tesseract unavailable");
        return;
    }
    let out = run(
        &ctx,
        serde_json::from_value(json!({"action":"ocr","reference":parent,"max_words":20})).unwrap(),
        &AtomicBool::new(false),
    )
    .unwrap();
    let result = &out.data.as_ref().unwrap()["frames"][0];
    let words = result["ocr"]["words"].as_array().unwrap();
    assert!(
        words
            .iter()
            .any(|w| w["text"].as_str().unwrap().contains("DIAL")),
        "actual OCR words: {words:?}"
    );
    assert_eq!(result["page"], 2);
    assert!(words.iter().all(|w| w["confidence"].as_f64().is_some()));
    eprintln!("LIVE tesseract: {words:?}");
}
