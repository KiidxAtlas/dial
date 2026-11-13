//! Parsed-catalog cache and persistence of discovered model listings.

use super::MAX_PROVIDERS;
use super::metadata::{ModelEntry, documented_context_window, documented_image_generation};
use super::probe::Probe;
use crate::config::settings_store::{locked, write_private};
use crate::llm::model_config::{ApiProtocol, ModelSelection};
use anyhow::Result;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

pub(super) fn selection_cache_key(selection: &ModelSelection) -> String {
    use sha2::{Digest, Sha256};
    let scope = serde_json::json!([
        "dial-model-catalog-v2",
        selection.provider,
        selection
            .base_url
            .as_deref()
            .map(|s| s.trim_end_matches('/')),
        selection
            .protocol
            .unwrap_or_else(|| ApiProtocol::builtin(&selection.provider)),
        selection.api_key_env,
        selection.headers.as_deref().unwrap_or_default()
    ]);
    format!("{:x}", Sha256::digest(scope.to_string().as_bytes()))
}
pub(super) fn probe_cache_key(probe: &Probe) -> String {
    selection_cache_key(&ModelSelection {
        provider: probe.id.clone(),
        base_url: Some(probe.spec.base_url.clone()),
        protocol: probe.spec.protocol,
        api_key_env: probe.spec.api_key_env.clone(),
        headers: probe.spec.headers.clone(),
        ..Default::default()
    })
}
/// Identity of the catalog file's current contents. Writes publish by rename (a new inode on Unix),
/// so a changed file never matches a cached parse even within one mtime tick.
fn catalog_stamp(path: &Path) -> Option<(u64, std::time::SystemTime, u64)> {
    let metadata = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let inode = std::os::unix::fs::MetadataExt::ino(&metadata);
    #[cfg(not(unix))]
    let inode = 0;
    Some((metadata.len(), metadata.modified().ok()?, inode))
}
type CatalogStamp = (std::path::PathBuf, (u64, std::time::SystemTime, u64));
type ParsedCatalog = (CatalogStamp, Arc<BTreeMap<String, Vec<ModelEntry>>>);
pub(super) static PARSED: std::sync::Mutex<Option<ParsedCatalog>> = std::sync::Mutex::new(None);
/// The saved catalog, parsed once per file version. Role and backend construction read it on
/// every request path; reparsing and re-keying every row each time was pure repeated work.
pub(super) fn cached(user_dir: &Path) -> Arc<BTreeMap<String, Vec<ModelEntry>>> {
    let path = crate::config::paths::model_catalog_file(user_dir);
    let Some(stamp) = catalog_stamp(&path).map(|stamp| (path.clone(), stamp)) else {
        return Arc::new(parse_cached(user_dir));
    };
    if let Some((seen, parsed)) = PARSED.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
        && *seen == stamp
    {
        return parsed.clone();
    }
    let parsed = Arc::new(parse_cached(user_dir));
    // Only remember a parse of the version that was stamped, never one that changed underneath.
    if catalog_stamp(&path) == Some(stamp.1) {
        *PARSED.lock().unwrap_or_else(|e| e.into_inner()) = Some((stamp, parsed.clone()));
    }
    parsed
}
fn parse_cached(user_dir: &Path) -> BTreeMap<String, Vec<ModelEntry>> {
    let mut cache: BTreeMap<String, Vec<ModelEntry>> =
        crate::platform::atomic::read_private(&crate::config::paths::model_catalog_file(user_dir))
            .ok()
            .flatten()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
    // Repair rows saved before a model was documented, without a network request or a rewrite:
    // missing windows in older Claude rows, and image output an older catalog did not know.
    for entries in cache.values_mut() {
        for entry in entries {
            if entry.selection.context_window.is_none() {
                entry.selection.context_window = documented_context_window(&entry.selection);
            }
            if entry.metadata.image_generation.is_none()
                && let Some(base_url) = entry.selection.base_url.as_deref()
            {
                entry.metadata.image_generation = documented_image_generation(
                    &entry.selection.provider,
                    base_url,
                    &entry.selection.model,
                );
            }
        }
    }
    // Migrate legacy URL-only caches by the declared policy of each row.
    let mut scoped: BTreeMap<String, Vec<ModelEntry>> = BTreeMap::new();
    for entry in cache.into_values().flatten() {
        scoped
            .entry(selection_cache_key(&entry.selection))
            .or_default()
            .push(entry);
    }
    scoped
}
pub(super) fn persist_cache(
    user_dir: &Path,
    updates: BTreeMap<String, Vec<ModelEntry>>,
) -> Result<()> {
    if updates.is_empty() {
        return Ok(());
    }
    let path = crate::config::paths::model_catalog_file(user_dir);
    let _lock = locked(&path)?;
    let mut cache = (*cached(user_dir)).clone();
    let fresh: Vec<String> = updates.keys().cloned().collect();
    cache.extend(updates);
    while cache.len() > MAX_PROVIDERS {
        let victim = cache
            .keys()
            .find(|key| !fresh.contains(key))
            .or_else(|| cache.keys().next())
            .cloned();
        match victim {
            Some(key) => {
                cache.remove(&key);
            }
            None => break,
        }
    }
    write_private(&path, &serde_json::to_vec(&cache)?)
}
pub(super) fn same_model(a: &ModelSelection, b: &ModelSelection) -> bool {
    a.provider == b.provider
        && a.model == b.model
        && a.base_url == b.base_url
        && a.protocol
            .unwrap_or_else(|| ApiProtocol::builtin(&a.provider))
            == b.protocol
                .unwrap_or_else(|| ApiProtocol::builtin(&b.provider))
        && a.account_id == b.account_id
        && a.headers.as_deref().unwrap_or_default() == b.headers.as_deref().unwrap_or_default()
        && a.api_key_env == b.api_key_env
}
/// Every model in the provider listings a `/models` refresh saved, across providers. No network
/// access; a model that was never listed (or a listing that was never refreshed) is absent.
pub fn saved_listing_models(user_dir: &Path) -> Vec<ModelEntry> {
    cached(user_dir).values().flatten().cloned().collect()
}
