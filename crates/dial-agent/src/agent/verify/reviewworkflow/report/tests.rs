use super::*;

fn file(name: &str, body: usize) -> String {
    format!(
        "diff --git a/{name} b/{name}\n--- a/{name}\n+++ b/{name}\n@@ -1 +1 @@\n+{}\n",
        "x".repeat(body)
    )
}

fn finding(priority: u64, confidence: f64, path: &str, title: &str) -> Value {
    json!({"priority":priority,"confidence":confidence,"path":path,"start_line":3,"end_line":4,"title":title,"body":"first\nsecond"})
}

fn chunk(verdict: &str, findings: Vec<Value>) -> ChunkReview {
    ChunkReview {
        range: 0..1,
        verdict: verdict.into(),
        summary: String::new(),
        findings,
    }
}

#[test]
fn chunks_pack_whole_files_and_cover_every_byte() {
    let text = [file("a.rs", 30), file("b.rs", 30), file("c.rs", 30)].concat();
    let one = file("a.rs", 30).len();
    let chunks = chunk_diff(&text, one * 2);
    assert_eq!(chunks, vec![0..one * 2, one * 2..text.len()]);
    for range in &chunks {
        assert!(text[range.clone()].starts_with("diff --git "));
    }
    assert_eq!(chunk_diff(&text, text.len()), vec![0..text.len()]);
    assert!(chunk_diff("", 10).is_empty());
}

#[test]
fn only_an_oversized_file_is_split_by_bytes() {
    let big = file("big.rs", 300);
    let small = file("small.rs", 10);
    let text = format!("{small}{big}{small}");
    let chunks = chunk_diff(&text, 100);
    assert_eq!(chunks[0], 0..small.len());
    let big_end = small.len() + big.len();
    assert!(
        chunks
            .iter()
            .filter(|r| r.start >= small.len() && r.end <= big_end)
            .all(|r| r.len() <= 100)
    );
    assert!(chunks.iter().filter(|r| r.end <= big_end).count() >= 3);
    assert_eq!(chunks.last().unwrap(), &(big_end..text.len()));
    let mut at = 0;
    for range in &chunks {
        assert_eq!(range.start, at);
        at = range.end;
    }
    assert_eq!(at, text.len());
}

#[test]
fn byte_split_respects_character_boundaries() {
    let text = format!("diff --git a/u b/u\n+{}\n", "é".repeat(50));
    let chunks = chunk_diff(&text, 31);
    let mut at = 0;
    for range in chunks {
        assert_eq!(range.start, at);
        assert!(text.is_char_boundary(range.end));
        at = range.end;
    }
    assert_eq!(at, text.len());
}

#[test]
fn merge_drops_exact_duplicates_and_sorts() {
    let chunks = [
        chunk(
            "findings",
            vec![
                finding(2, 0.9, "a.rs", "dup"),
                finding(1, 0.4, "b.rs", "low confidence"),
                finding(3, 0.99, "a.rs", "nit"),
            ],
        ),
        chunk(
            "findings",
            vec![
                finding(2, 0.5, "a.rs", "dup"),
                finding(1, 0.8, "c.rs", "high confidence"),
                finding(2, 0.5, "a.rs", "different title"),
            ],
        ),
    ];
    let merged = Merged::new(&chunks, true);
    let order: Vec<_> = merged
        .findings
        .iter()
        .map(|f| f["title"].as_str().unwrap())
        .collect();
    assert_eq!(
        order,
        [
            "high confidence",
            "low confidence",
            "dup",
            "different title",
            "nit"
        ]
    );
    assert_eq!(merged.findings[2]["confidence"], 0.9);
    assert_eq!(merged.counts, [0, 2, 2, 1]);
    assert!(
        merged
            .findings
            .iter()
            .all(|f| f["stale_snapshot"] == json!(true))
    );
}

#[test]
fn verdict_rules() {
    let p = |level| chunk("findings", vec![finding(level, 0.5, "a.rs", "t")]);
    let verdict = |chunks: &[ChunkReview]| Merged::new(chunks, false).verdict;
    assert_eq!(verdict(&[chunk("no_findings", vec![])]), Verdict::Ship);
    assert_eq!(verdict(&[]), Verdict::Ship);
    assert_eq!(verdict(&[p(3)]), Verdict::ShipWithFixes);
    assert_eq!(
        verdict(&[p(2), chunk("no_findings", vec![])]),
        Verdict::ShipWithFixes
    );
    assert_eq!(verdict(&[p(1)]), Verdict::DoNotShip);
    assert_eq!(verdict(&[p(0)]), Verdict::DoNotShip);
    let bad = ChunkReview::failed(0..1, "timeout");
    assert_eq!(
        verdict(&[chunk("no_findings", vec![]), bad]),
        Verdict::Inconclusive
    );
    let bad = || ChunkReview::failed(0..1, "timeout");
    assert_eq!(verdict(&[p(2), bad()]), Verdict::Inconclusive);
    assert_eq!(verdict(&[p(1), bad()]), Verdict::DoNotShip);
    assert_eq!(verdict(&[p(0), bad()]), Verdict::DoNotShip);
    assert_eq!(
        verdict(&[chunk("inconclusive", vec![])]),
        Verdict::Inconclusive
    );
}

#[test]
fn report_is_readable_and_json_stays_structured() {
    let mut failed = ChunkReview::failed(5..9, "reviewer failed: boom");
    failed.verdict = "inconclusive".into();
    let chunks = [
        chunk(
            "findings",
            vec![
                finding(3, 0.3, "z.rs", "nit"),
                finding(0, 0.95, "a.rs", "data loss"),
            ],
        ),
        failed,
    ];
    let merged = Merged::new(&chunks, false);
    let text = render("local:abc", false, &chunks, &merged);
    assert!(text.starts_with("Target local:abc\nTarget identity unchanged"));
    assert!(text.contains("Verdict: do not ship"));
    assert!(text.contains("Findings: 2 (P0 1, P1 0, P2 0, P3 1) from 2 chunk(s), 1 inconclusive"));
    assert!(text.find("P0 critical (1)").unwrap() < text.find("P3 low (1)").unwrap());
    assert!(!text.contains("P1 high"));
    assert!(text.contains("1. data loss (a.rs:3-4, confidence 0.95)"));
    assert!(text.contains("     second"));
    assert!(text.contains("reviewer failed: boom"));
    assert!(render("t", true, &chunks, &merged).contains("WARNING: target changed"));

    let report = to_json("local:abc", false, &chunks, &merged);
    assert_eq!(report["verdict"], "do_not_ship");
    assert_eq!(report["counts"], json!({"P0":1,"P1":0,"P2":0,"P3":1}));
    assert_eq!(report["findings"][0]["title"], "data loss");
    assert_eq!(report["findings"][0]["stale_snapshot"], false);
    assert_eq!(report["chunks"][1]["bytes"], json!([5, 9]));
}

#[test]
fn reviewer_answers_are_validated_per_chunk() {
    let diff = file("a.rs", 5);
    let answer =
        json!({"verdict":"findings","summary":"s","findings":[finding(1, 0.7, "a.rs", "bug")]});
    let job = json!({"status":"done","result":answer});
    let ok = from_job(0..1, &job, &diff, "id");
    assert_eq!(ok.findings.len(), 1);
    assert_eq!(ok.findings[0]["target_identity"], "id");
    let as_text = json!({"status":"done","result":answer.to_string()});
    assert_eq!(from_job(0..1, &as_text, &diff, "id").findings.len(), 1);

    for bad in [
        json!({"verdict":"findings","summary":"","findings":[finding(1, 0.7, "other.rs", "bug")]}),
        json!({"verdict":"findings","summary":"","findings":[finding(1, 0.7, "../a.rs", "bug")]}),
        json!({"verdict":"no_findings","summary":"","findings":[finding(1, 0.7, "a.rs", "bug")]}),
        json!({"verdict":"findings","summary":"","findings":[{"priority":1,"confidence":0.5,"path":"a.rs","start_line":9,"end_line":2,"title":"t","body":"b"}]}),
    ] {
        let review = from_job(0..1, &json!({"status":"done","result":bad}), &diff, "id");
        assert!(review.inconclusive() && review.findings.is_empty());
        assert!(review.summary.contains("rejected"));
    }
    let failed = from_job(
        0..1,
        &json!({"status":"failed","result":"boom"}),
        &diff,
        "id",
    );
    assert!(failed.inconclusive());
    assert!(failed.summary.contains("boom"));
    assert!(from_job(0..1, &Value::Null, &diff, "id").inconclusive());
}
