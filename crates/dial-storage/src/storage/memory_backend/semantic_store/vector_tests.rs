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
