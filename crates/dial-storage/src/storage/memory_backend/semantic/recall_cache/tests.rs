use super::*;

fn recall() -> Recall {
    Recall {
        facts: Vec::new(),
        truncated: false,
        mode: "lexical",
        voices: vec!["lexical"],
        embedding_error: None,
    }
}

fn key(query: &str) -> Key {
    Key {
        query: query.into(),
        limit: 8,
        token_budget: 1500,
        vector: None,
    }
}

#[test]
fn an_entry_is_served_only_while_the_write_token_is_unchanged() {
    let mut cache = RecallCache::default();
    let now = Instant::now();
    cache.put(key("deploy"), recall(), (1, 0), now);
    assert!(cache.get(&key("deploy"), (1, 0), now).is_some());
    assert!(cache.get(&key("deploy"), (1, 1), now).is_none());
    assert_eq!(cache.stats().entries, 0);
}

#[test]
fn an_entry_expires_after_the_ttl() {
    let mut cache = RecallCache::default();
    let stored = Instant::now();
    cache.put(key("deploy"), recall(), (1, 0), stored);
    let just_before = stored + TTL - Duration::from_secs(1);
    assert!(cache.get(&key("deploy"), (1, 0), just_before).is_some());
    assert!(cache.get(&key("deploy"), (1, 0), stored + TTL).is_none());
}

#[test]
fn the_cache_evicts_the_oldest_entry_beyond_its_bound() {
    let mut cache = RecallCache::default();
    let now = Instant::now();
    for index in 0..=MAX_ENTRIES {
        cache.put(key(&index.to_string()), recall(), (1, 0), now);
    }
    assert_eq!(cache.stats().entries, MAX_ENTRIES);
    assert!(cache.get(&key("0"), (1, 0), now).is_none());
    assert!(
        cache
            .get(&key(&MAX_ENTRIES.to_string()), (1, 0), now)
            .is_some()
    );
}

#[test]
fn a_hit_protects_its_entry_from_eviction() {
    let mut cache = RecallCache::default();
    let now = Instant::now();
    for index in 0..MAX_ENTRIES {
        cache.put(key(&index.to_string()), recall(), (1, 0), now);
    }
    assert!(cache.get(&key("0"), (1, 0), now).is_some());
    cache.put(key("extra"), recall(), (1, 0), now);
    assert!(cache.get(&key("0"), (1, 0), now).is_some());
    assert!(cache.get(&key("1"), (1, 0), now).is_none());
    assert_eq!(
        cache.stats(),
        CacheStats {
            entries: MAX_ENTRIES,
            hits: 2,
            misses: 1,
        }
    );
}

#[test]
fn a_different_query_vector_is_a_different_entry() {
    let mut cache = RecallCache::default();
    let now = Instant::now();
    let mut embedded = key("deploy");
    embedded.vector = Some(("model".into(), vec![1.0f32.to_bits()]));
    cache.put(key("deploy"), recall(), (1, 0), now);
    assert!(cache.get(&embedded, (1, 0), now).is_none());
}
