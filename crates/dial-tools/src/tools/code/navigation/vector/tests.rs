use super::*;
use anyhow::Result;

/// A deterministic embedder: counts of a few topic words, so similarity is predictable.
struct Topics {
    calls: usize,
}

const TOPICS: [&str; 4] = ["retry", "cache", "parse", "render"];

impl Embedder for Topics {
    fn model(&self) -> &str {
        "topics"
    }
    fn embed(&mut self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        self.calls += 1;
        Ok(inputs
            .iter()
            .map(|text| {
                let lower = text.to_lowercase();
                TOPICS
                    .iter()
                    .map(|topic| lower.matches(topic).count() as f32 + 0.01)
                    .collect()
            })
            .collect())
    }
}

#[test]
fn windows_overlap_and_skip_blank_spans() {
    let text: String = (1..=100).map(|n| format!("line number {n}\n")).collect();
    assert_eq!(windows(&text), vec![(1, 40), (31, 70), (61, 100)]);
    assert!(windows("\n\n  \n").is_empty());
    assert_eq!(windows("fn short_but_long_enough() {}\n"), vec![(1, 1)]);
}

#[test]
fn cosine_is_bounded_and_rejects_mismatched_dimensions() {
    assert!((cosine(&[1.0, 0.0], &[2.0, 0.0]) - 1.0).abs() < 1e-6);
    assert_eq!(cosine(&[1.0, 0.0], &[0.0, 1.0]), 0.0);
    assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0);
    assert_eq!(cosine(&[0.0], &[0.0]), 0.0);
}

#[test]
fn ranks_by_similarity_and_reuses_cached_vectors() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("net.rs"),
        "fn send() { retry retry retry backoff; }\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("store.rs"),
        "fn get() { cache cache lookup and insert; }\n",
    )
    .unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let mut embedder = Topics { calls: 0 };
    let args = serde_json::json!({"query": "where do we retry a failed request", "limit": 1});
    let out = search(&ws, &args, &mut embedder, "test endpoint").unwrap();
    assert!(out.contains("net.rs:1-1 similarity="), "{out}");
    assert!(!out.contains("store.rs"), "{out}");
    assert!(
        out.contains("2 of 2 windows indexed (2 embedded this call)"),
        "{out}"
    );
    // Second call: only the query is embedded.
    let before = embedder.calls;
    let out = search(
        &ws,
        &serde_json::json!({"query": "cache lookup", "limit": 1}),
        &mut embedder,
        "test endpoint",
    )
    .unwrap();
    assert!(out.contains("store.rs:1-1"), "{out}");
    assert!(out.contains("(0 embedded this call)"), "{out}");
    assert_eq!(embedder.calls, before + 1);

    let error = search(
        &ws,
        &serde_json::json!({"query": "x", "mode": "exact"}),
        &mut embedder,
        "e",
    )
    .unwrap_err();
    assert!(error.contains("behavior mode only"), "{error}");
}
