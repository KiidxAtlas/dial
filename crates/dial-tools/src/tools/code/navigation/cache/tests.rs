use super::*;

fn entry(cache: &Cache) -> Entry {
    Entry {
        identity: Identity {
            size: 0,
            #[cfg(unix)]
            dev: 0,
            #[cfg(unix)]
            ino: 0,
            #[cfg(unix)]
            mtime: (0, 0),
            #[cfg(unix)]
            ctime: (0, 0),
            #[cfg(not(unix))]
            modified: None,
            #[cfg(not(unix))]
            created: None,
        },
        revision: [0; 32],
        source: Arc::new(Source::new("f", "x")),
        bytes: 1,
        stamp: 0,
        sweep: cache.sweep,
    }
}

fn key(index: usize) -> Key {
    (
        PathBuf::from("/root"),
        PathBuf::from(format!("/root/{index}")),
    )
}

#[test]
fn a_sweep_larger_than_the_cache_keeps_hitting_what_earlier_sweeps_stored() {
    let mut cache = Cache {
        sweep: 1,
        ..Cache::default()
    };
    // First sweep over more files than fit: the cache fills, then stops admitting.
    for index in 0..MAX_CACHE_ENTRIES + 100 {
        if cache.make_room(1) {
            let value = entry(&cache);
            cache.insert(key(index), value);
        }
    }
    assert_eq!(cache.entries.len(), MAX_CACHE_ENTRIES);
    // The next sweep finds the stored prefix still there (plain LRU would have evicted it).
    cache.sweep = 2;
    let mut hits = 0;
    for index in 0..MAX_CACHE_ENTRIES + 100 {
        if let Some(found) = cache.remove(&key(index)) {
            hits += 1;
            cache.insert(key(index), found);
        } else if cache.make_room(1) {
            let value = entry(&cache);
            cache.insert(key(index), value);
        }
    }
    assert_eq!(hits, MAX_CACHE_ENTRIES);
}

#[test]
fn entries_unused_in_the_current_sweep_are_evicted_first() {
    let mut cache = Cache {
        sweep: 1,
        ..Cache::default()
    };
    for index in 0..MAX_CACHE_ENTRIES {
        let value = entry(&cache);
        cache.insert(key(index), value);
    }
    cache.sweep = 2;
    // Use one old entry in this sweep; a new file then displaces an unused one, not it.
    let used = cache.remove(&key(0)).unwrap();
    cache.insert(key(0), used);
    assert!(cache.make_room(1));
    assert!(cache.entries.contains_key(&key(0)));
    assert!(!cache.entries.contains_key(&key(1)));
}
