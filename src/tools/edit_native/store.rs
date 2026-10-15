//! Session no-op escalation for the pure-source engine. Not a source or permission store.
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Mutex,
};
#[derive(Debug, Default)]
pub struct EditStore {
    noops: Mutex<BTreeMap<PathBuf, (u64, u32)>>,
}
impl EditStore {
    pub fn record_noop(&self, path: &Path, payload: u64) -> (u32, bool) {
        let mut store = self.noops.lock().unwrap_or_else(|e| e.into_inner());
        let count = store
            .get(path)
            .filter(|(hash, _)| *hash == payload)
            .map_or(1, |(_, n)| n.saturating_add(1));
        if store.len() >= 128 && !store.contains_key(path) {
            store.pop_first();
        }
        store.insert(path.to_owned(), (payload, count));
        (count, count >= 3)
    }
    pub fn reset_noop(&self, path: &Path) {
        self.noops
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(path);
    }
}
pub fn payload_hash(input: &str) -> u64 {
    u64::from_le_bytes(Sha256::digest(input.as_bytes())[..8].try_into().unwrap())
}
