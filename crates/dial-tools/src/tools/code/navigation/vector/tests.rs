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

#[test]
fn cache_does_not_cross_embedding_routes() {
    struct Routed(Topics, &'static str);
    impl Embedder for Routed {
        fn model(&self) -> &str {
            self.0.model()
        }
        fn route(&self) -> String {
            self.1.into()
        }
        fn embed(&mut self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
            self.0.embed(inputs)
        }
    }
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("route.rs"),
        "fn retry_route_isolation_regression() {}\n",
    )
    .unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let args = serde_json::json!({"query": "retry"});
    for route in ["isolated-route-a", "isolated-route-b"] {
        let mut embedder = Routed(Topics { calls: 0 }, route);
        let out = search(&ws, &args, &mut embedder, "test").unwrap();
        assert!(out.contains("(1 embedded this call)"), "{route}: {out}");
    }
}

#[test]
fn walker_depth_limit_is_reported_as_incomplete() {
    let dir = tempfile::tempdir().unwrap();
    let mut path = dir.path().to_path_buf();
    for _ in 0..41 {
        path.push("d");
    }
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("deep.rs"), "fn retry_deep_function() {}\n").unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let out = search(
        &ws,
        &serde_json::json!({"query": "retry"}),
        &mut Topics { calls: 0 },
        "test",
    )
    .unwrap();
    assert!(out.contains("scope capped"), "{out}");
}

#[test]
fn source_descriptor_enforces_byte_and_file_kind_bounds() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.rs");
    fs::write(&source, "12345").unwrap();
    for (limit, admitted) in [(0, false), (4, false), (5, true), (6, true)] {
        let result = read_source(&source, limit);
        assert_eq!(result.is_ok(), admitted, "limit={limit}: {result:?}");
        if let Ok(data) = result {
            assert_eq!(data, b"12345");
        }
    }
    assert!(read_source(dir.path(), 1024).is_err());
    #[cfg(unix)]
    {
        let link = dir.path().join("link.rs");
        std::os::unix::fs::symlink(&source, &link).unwrap();
        assert!(
            read_source(&link, 1024).is_err(),
            "must not follow a swapped final component"
        );
        let fifo = dir.path().join("fifo");
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: name is a valid NUL-terminated path; mkfifo retains no pointer.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(
            read_source(&fifo, 1024).is_err(),
            "must not block on or read a FIFO"
        );
    }
}

#[test]
fn cache_evicts_incrementally_under_byte_pressure() {
    let mut cached = VectorCache::default();
    let vector: Arc<[f32]> = vec![1.0; 16].into();
    let key = |n| ("eviction-route".to_owned(), [n; 32]);
    let weight = VectorCache::weight(&key(0), &vector);
    let budget = weight * 3;
    for n in 0..4 {
        cached.insert(key(n), Arc::clone(&vector), budget);
    }
    assert!(
        !cached.entries.contains_key(&key(0)),
        "oldest entry must be evicted"
    );
    for n in 1..4 {
        assert!(
            cached.entries.contains_key(&key(n)),
            "must retain unaffected entries"
        );
    }
    assert!(cached.bytes <= budget);
    let oversized: Arc<[f32]> = vec![1.0; budget].into();
    cached.insert(key(9), oversized, budget);
    assert_eq!(
        cached.entries.len(),
        3,
        "oversized vectors must not flush useful entries"
    );
    assert!(!cached.entries.contains_key(&key(9)));
    cached.insert(key(1), vector, budget);
    assert_eq!(
        cached.bytes, budget,
        "duplicate inserts must not double-account"
    );
}

#[test]
fn repeated_search_reaches_tail_despite_cache_eviction() {
    let dir = tempfile::tempdir().unwrap();
    let body: String = (0..(STEP * MAX_NEW * 2))
        .map(|n| format!("fn cache_item_{n}() {{}}\n"))
        .collect();
    fs::write(
        dir.path().join("large.rs"),
        format!("{body}{}", "render distinctive final window\n".repeat(31)),
    )
    .unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let args = serde_json::json!({"query": "render", "limit": 1});
    for budget in [0, 1024] {
        let cache = Mutex::new(VectorCache::default());
        let mut embedder = Topics { calls: 0 };
        let mut reached_tail = false;
        for _ in 0..3 {
            let indexed = prepare(&ws, &args)
                .unwrap()
                .embed_with_cache(&mut embedder, &cache, budget)
                .unwrap();
            reached_tail |= indexed.vectors.last().unwrap().is_some();
            assert!(cache.lock().unwrap().bytes <= budget);
            let out = indexed.render("test").unwrap();
            assert!(out.contains("windows not indexed this call"), "{out}");
            if reached_tail {
                assert!(out.contains("distinctive final window"), "{out}");
                break;
            }
        }
        assert!(
            reached_tail,
            "budget={budget}: repeated searches must eventually embed the final window"
        );
    }
}

#[test]
fn progress_metadata_is_bounded_and_scopes_advance_independently() {
    let mut cache = VectorCache::default();
    let vectors = vec![None; MAX_NEW + 1];
    for scope in 0..MAX_SCOPES + 1 {
        let key = Sha256::digest(scope.to_le_bytes()).into();
        assert_eq!(cache.select_missing(key, &vectors).first(), Some(&0));
        assert!(cache.progress.len() <= MAX_SCOPES);
    }
    let recent = Sha256::digest(MAX_SCOPES.to_le_bytes()).into();
    assert_eq!(
        cache.select_missing(recent, &vectors).first(),
        Some(&MAX_NEW),
        "recent scopes retain progress despite other scope searches"
    );
    let evicted = Sha256::digest(0usize.to_le_bytes()).into();
    assert_eq!(
        cache.select_missing(evicted, &vectors).first(),
        Some(&0),
        "an evicted scope restarts instead of retaining unbounded history"
    );
}
