use super::*;

fn store() -> Store {
    Store::new(Connection::open_in_memory().unwrap()).unwrap()
}

fn reference(store: &Store, model: &str, query: &[f32], limit: usize) -> Vec<i64> {
    let mut statement = store
        .conn
        .prepare(
            "SELECT f.id,e.vector,e.dimensions FROM facts f JOIN embeddings e
         ON e.content_hash=f.content_hash AND e.model=?1
         WHERE f.forgotten=0 ORDER BY f.id DESC LIMIT ?2",
        )
        .unwrap();
    let rows = statement
        .query_map(params![model, MAX_VECTOR_ROWS as i64], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .unwrap();
    let mut scored: Vec<_> = rows
        .map(|row| {
            let (id, bytes, dimensions) = row.unwrap();
            (id, decode(&bytes, dimensions as usize).unwrap())
        })
        .filter(|(_, vector)| vector.len() == query.len())
        .map(|(id, vector)| (id, cosine(query, &vector)))
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(b.0.cmp(&a.0)));
    scored.into_iter().take(limit).map(|(id, _)| id).collect()
}

#[test]
fn streaming_matches_full_sort_across_queries_limits_and_mutations() {
    let mut store = store();
    let mut ids = Vec::new();
    for index in 0..80 {
        let content = format!("fact {index}");
        let fact = match store.retain(&content, "test", index).unwrap() {
            Saved::Created(fact) => fact,
            _ => unreachable!(),
        };
        ids.push(fact.id);
        let vector = match index % 8 {
            0 | 1 => vec![1.0, 0.0], // exact ties favor the newer id
            2 => vec![0.0, 0.0],
            3 => vec![-1.0, 0.0],
            4 => vec![1.0], // wrong dimension
            5 => vec![f32::MAX, f32::MAX],
            6 => vec![f32::NAN, 1.0],
            _ => vec![index as f32, 1.0],
        };
        store
            .store_vectors("a", &[(content.clone(), vector)])
            .unwrap();
        store
            .store_vectors("b", &[(content, vec![0.0, 1.0])])
            .unwrap();
    }
    store.forget(ids[1], 1, 100).unwrap();
    store
        .update(ids[2], 1, "corrected fact", "test", "fix", 100)
        .unwrap();
    for model in ["a", "b", "missing"] {
        for query in [
            vec![1.0, 0.0],
            vec![0.0, 1.0],
            vec![0.0, 0.0],
            vec![1.0],
            vec![],
        ] {
            for limit in [0, 1, 3, 20, 200, usize::MAX] {
                assert_eq!(
                    store.nearest(model, &query, limit).unwrap(),
                    reference(&store, model, &query, limit)
                );
            }
        }
    }
    assert!(
        !store
            .nearest("a", &[1.0, 0.0], 200)
            .unwrap()
            .contains(&ids[1])
    );
    assert!(
        !store
            .nearest("a", &[1.0, 0.0], 200)
            .unwrap()
            .contains(&ids[2])
    );
}

#[test]
fn corrupt_rows_still_fail_instead_of_silently_changing_ranks() {
    let mut store = store();
    store.retain("broken", "test", 1).unwrap();
    store
        .store_vectors("a", &[("broken".into(), vec![1.0])])
        .unwrap();
    for dimensions in [2i64, -1, i64::MAX] {
        store
            .conn
            .execute("UPDATE embeddings SET dimensions=?1", [dimensions])
            .unwrap();
        assert!(
            store
                .nearest("a", &[1.0, 0.0], 1)
                .unwrap_err()
                .to_string()
                .contains("corrupt embedding row")
        );
    }
}

#[test]
fn scan_keeps_the_existing_newest_row_cap() {
    let mut store = store();
    let tx = store.conn.transaction().unwrap();
    for id in 1..=MAX_VECTOR_ROWS + 1 {
        // Distinct hashes need not be real digests for this SQL-only fixture.
        let hash = id.to_string();
        tx.execute("INSERT INTO facts(id,content,content_hash,source,revision,created,updated) VALUES(?1,'fixture',?2,'test',1,1,1)", params![id as i64, hash]).unwrap();
        let vector = if id == 1 {
            vec![1.0, 0.0]
        } else {
            vec![0.0, 1.0]
        };
        tx.execute(
            "INSERT INTO embeddings VALUES(?1,'a',2,?2)",
            params![hash, encode(&vector)],
        )
        .unwrap();
    }
    tx.commit().unwrap();
    let found = store.nearest("a", &[1.0, 0.0], 3).unwrap();
    assert_eq!(
        found,
        vec![
            MAX_VECTOR_ROWS as i64 + 1,
            MAX_VECTOR_ROWS as i64,
            MAX_VECTOR_ROWS as i64 - 1
        ]
    );
}

#[test]
fn retained_scores_are_bounded_independently_of_scanned_rows() {
    let limit = 20;
    let mut heap = BinaryHeap::with_capacity(limit);
    for id in 1..=MAX_VECTOR_ROWS as i64 {
        keep_nearest(
            &mut heap,
            VectorMatch {
                id,
                score: (id % 17) as f32,
            },
            limit,
        );
        assert!(heap.len() <= limit);
        assert_eq!(heap.capacity(), limit);
    }
    assert_eq!(heap.len(), limit);
    // At the production cap and 1536 dimensions, the old decoded-vector payload
    // alone was 122,880,000 bytes. The new retained payload is 20 score/id pairs
    // (320 bytes on 64-bit targets) plus one 6144-byte vector, independent of rows.
    assert!(heap.capacity() * std::mem::size_of::<VectorMatch>() <= 640);
}

#[test]
fn simhash_preserves_angular_ordering_and_recalls_oldest_fact_beyond_exact_threshold() {
    let a = [1.0f32, 0.0, 0.25, -0.5, 0.75, 0.1, -0.2, 0.9];
    let near = [0.98f32, 0.02, 0.24, -0.49, 0.74, 0.11, -0.19, 0.89];
    let opposite: Vec<f32> = a.iter().map(|v| -v).collect();
    let sig_a = simhash(&a);
    assert_eq!((sig_a ^ simhash(&a)).count_ones(), 0);
    assert!((sig_a ^ simhash(&near)).count_ones() <= 4);
    assert_eq!((sig_a ^ simhash(&opposite)).count_ones(), 64);

    let mut store = store();
    let target = match store.retain("oldest needle fact", "test", 1).unwrap() {
        Saved::Created(fact) => fact,
        _ => unreachable!(),
    };
    store
        .store_vectors("m", &[("oldest needle fact".into(), a.to_vec())])
        .unwrap();
    let mut batch = Vec::new();
    for i in 0..300 {
        let content = format!("newer haystack fact {i}");
        store.retain(&content, "test", i + 2).unwrap();
        // Orthogonal/opposite-ish vectors that should never outrank `a`.
        let v = vec![-0.5, 1.0, (i as f32 % 7.0) * 0.1, 0.8, -0.6, 0.4, 0.9, -0.7];
        batch.push((content, v));
    }
    store.store_vectors("m", &batch).unwrap();
    // With exact_threshold=50 over 301 rows, the old newest-only cap would have dropped `target`.
    let found = store.nearest_with_threshold("m", &a, 3, 50).unwrap();
    assert_eq!(found.first().copied(), Some(target.id));
}

#[test]
fn lsh_banded_index_recalls_needle_directly_and_purges_on_forget() {
    let needle_vec = [1.0f32, 0.2, -0.3, 0.8, 0.5, -0.4, 0.7, -0.1];
    let near_vec = [0.99f32, 0.21, -0.29, 0.79, 0.51, -0.39, 0.69, -0.11];
    let mut store = store();
    let target = match store.retain("lsh needle fact", "test", 1).unwrap() {
        Saved::Created(fact) => fact,
        _ => unreachable!(),
    };
    let target_hash = content_hash(&target.content);
    store
        .store_vectors("m", &[("lsh needle fact".into(), needle_vec.to_vec())])
        .unwrap();
    let mut batch = Vec::new();
    for i in 0..80 {
        let content = format!("lsh neighbor {i}");
        store.retain(&content, "test", i + 2).unwrap();
        // First 20 are near neighbors so LSH probe returns >= 16 candidates without full fallback;
        // remaining 60 are distant haystack vectors.
        let v = if i < 20 {
            near_vec.to_vec()
        } else {
            vec![-1.0, -0.2, 0.3, -0.8, -0.5, 0.4, -0.7, 0.1]
        };
        batch.push((content, v));
    }
    store.store_vectors("m", &batch).unwrap();

    let band_rows: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM embedding_lsh WHERE model='m'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(band_rows, 81 * 4);

    // Direct LSH band probe must retrieve the oldest needle without scanning the full table.
    let lsh_hits = store
        .probe_lsh_candidates("m", simhash(&needle_vec), 256)
        .unwrap();
    assert!(
        lsh_hits.iter().any(|(id, _)| *id == target.id),
        "expected target id in LSH band hits"
    );
    assert!(
        lsh_hits.len() < 81,
        "LSH probe should skip distant opposite-direction facts"
    );

    let found = store
        .nearest_with_threshold("m", &needle_vec, 3, 25)
        .unwrap();
    assert_eq!(found.first().copied(), Some(target.id));

    // Forgetting the fact must purge all 4 LSH band entries immediately.
    store.forget(target.id, 1, 500).unwrap();
    let remaining_for_target: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM embedding_lsh WHERE content_hash=?1",
            [&target_hash],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(remaining_for_target, 0);
}

#[test]
fn distant_lsh_hot_bucket_collisions_do_not_drop_true_semantic_match() {
    let query = [1.0f32, 0.2, -0.3, 0.8, 0.5, -0.4, 0.7, -0.1];
    let query_sig = simhash(&query);
    let [q_b0, _, _, _] = Store::simhash_bands(query_sig);
    // Flip 1 bit in each of the 4 16-bit bands (Hamming = 4) so the needle misses exact LSH bands,
    // while 25 distant decoys share band 0 with Hamming = 24 to simulate hot-bucket collisions.
    let needle_sig =
        ((query_sig as u64) ^ ((1u64 << 0) | (1u64 << 16) | (1u64 << 32) | (1u64 << 48))) as i64;
    let decoy_sig = (((query_sig as u64) & 0xFFFF)
        | ((!((query_sig as u64) >> 16) & 0xFFFF_FFFF) << 16)) as i64;
    assert!((query_sig ^ decoy_sig).count_ones() >= 16);

    let mut store = store();
    let needle = match store.retain("true semantic match", "test", 1).unwrap() {
        Saved::Created(fact) => fact,
        _ => unreachable!(),
    };
    let needle_hash = content_hash(&needle.content);
    store
        .store_vectors("m", &[("true semantic match".into(), query.to_vec())])
        .unwrap();
    // Override needle's stored sig/LSH to simulate a 4-bit spread across all bands.
    store
        .conn
        .execute(
            "UPDATE embedding_sigs SET sig=?1 WHERE content_hash=?2 AND model='m'",
            params![needle_sig, &needle_hash],
        )
        .unwrap();
    store
        .conn
        .execute(
            "DELETE FROM embedding_lsh WHERE content_hash=?1",
            [&needle_hash],
        )
        .unwrap();
    for (band, bucket) in Store::simhash_bands(needle_sig).into_iter().enumerate() {
        store
            .conn
            .execute(
                "INSERT INTO embedding_lsh(model,band,bucket,content_hash) VALUES('m',?1,?2,?3)",
                params![band as i64, bucket, &needle_hash],
            )
            .unwrap();
    }

    // Insert 25 distant decoys sharing only band 0 (q_b0) with query.
    for i in 0..25 {
        let text = format!("distant hot-bucket decoy {i}");
        let hash = content_hash(&text);
        store.retain(&text, "test", i + 2).unwrap();
        store
            .store_vectors(
                "m",
                &[(text, vec![-1.0, -0.2, 0.3, -0.8, -0.5, 0.4, -0.7, 0.1])],
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE embedding_sigs SET sig=?1 WHERE content_hash=?2 AND model='m'",
                params![decoy_sig, &hash],
            )
            .unwrap();
        store
            .conn
            .execute("DELETE FROM embedding_lsh WHERE content_hash=?1", [&hash])
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO embedding_lsh(model,band,bucket,content_hash) VALUES('m',0,?1,?2)",
                params![q_b0, &hash],
            )
            .unwrap();
    }

    // LSH returns 25 distant hits (>= min_lsh_hits=16), but must fall back and still recall `needle`.
    let found = store.nearest_with_threshold("m", &query, 3, 10).unwrap();
    assert_eq!(found.first().copied(), Some(needle.id));
}
