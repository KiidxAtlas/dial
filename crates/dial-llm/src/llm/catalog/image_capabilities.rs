//! Persistent cache of image-generation capabilities across providers.
//!
//! Stores verified runtime outcomes (successful `image_gen` calls and definitive provider/account
//! refusals) alongside metadata-declared, documented, and candidate image models discovered across
//! providers. Scoped by hashed route/account/model keys so raw URLs, headers, and account IDs are
//! never written into cache keys.

use super::bundled;
use super::metadata::{ModelEntry, ModelKind, documented_image_generation};
use crate::config::paths::{image_capabilities_file, subscription_auth_dir};
use crate::config::settings_store::{locked, write_private};
use crate::llm::chatgpt;
use crate::llm::model_config::{ApiProtocol, ModelSelection};
use crate::llm::profiles::ImageGenerationApi;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const CACHE_VERSION: u32 = 1;
const MAX_ENTRIES: usize = 2048;
const MAX_ACCOUNT_REFUSALS: usize = 128;

/// Cached image-generation capability of a model on a specific route/account.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImageCapabilityStatus {
    /// Confirmed capable by live generation, explicit provider metadata, or documented direct API.
    Capable,
    /// Eligible candidate (e.g. ChatGPT hosted image tool when the catalog omits the declaration);
    /// verified on the first approved generation.
    Candidate,
    /// Confirmed incapable by a live capability refusal or explicit provider disablement.
    Incapable,
}

impl ImageCapabilityStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Capable => "capable",
            Self::Candidate => "candidate",
            Self::Incapable => "incapable",
        }
    }
}

/// How the cached image-generation status was established.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImageCapabilitySource {
    /// Observed from a completed `image_gen` call or definitive refusal error.
    Verified,
    /// Declared by provider listing metadata or local checkpoint inspection.
    Metadata,
    /// Exact documented model ID on its official direct provider endpoint.
    Documented,
    /// Eligible provider route whose catalog does not declare image capability explicitly.
    Candidate,
}

impl ImageCapabilitySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Metadata => "metadata",
            Self::Documented => "documented",
            Self::Candidate => "candidate",
        }
    }
}

/// One persisted model capability record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedImageCapability {
    pub provider: String,
    pub model: String,
    pub status: ImageCapabilityStatus,
    pub source: ImageCapabilitySource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<ImageGenerationApi>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub checked_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct AccountRefusal {
    provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    checked_at: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct CacheFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    updated_at: u64,
    #[serde(default)]
    models: BTreeMap<String, CachedImageCapability>,
    #[serde(default)]
    refused_accounts: BTreeMap<String, AccountRefusal>,
}

type CacheStamp = (PathBuf, (u64, SystemTime, u64));
type ParsedCache = (CacheStamp, Arc<CacheFile>);
static PARSED: Mutex<Option<ParsedCache>> = Mutex::new(None);

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn file_stamp(path: &Path) -> Option<(u64, SystemTime, u64)> {
    let metadata = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let inode = std::os::unix::fs::MetadataExt::ino(&metadata);
    #[cfg(not(unix))]
    let inode = 0;
    Some((metadata.len(), metadata.modified().ok()?, inode))
}

fn load_cached(user_dir: &Path) -> Arc<CacheFile> {
    let path = image_capabilities_file(user_dir);
    let Some(stamp) = file_stamp(&path).map(|stamp| (path.clone(), stamp)) else {
        return Arc::new(CacheFile::default());
    };
    if let Some((seen, parsed)) = PARSED.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
        && *seen == stamp
    {
        return parsed.clone();
    }
    let parsed = Arc::new(parse_file(&path));
    if file_stamp(&path) == Some(stamp.1) {
        *PARSED.lock().unwrap_or_else(|e| e.into_inner()) = Some((stamp, parsed.clone()));
    }
    parsed
}

fn parse_file(path: &Path) -> CacheFile {
    crate::platform::atomic::read_private(path)
        .ok()
        .flatten()
        .and_then(|bytes| serde_json::from_slice::<CacheFile>(&bytes).ok())
        .filter(|file| file.version == 0 || file.version == CACHE_VERSION)
        .unwrap_or_default()
}

fn mutate_cache(user_dir: &Path, edit: impl FnOnce(&mut CacheFile) -> bool) -> Result<()> {
    let path = image_capabilities_file(user_dir);
    let _lock = locked(&path)?;
    let mut cache = parse_file(&path);
    if !edit(&mut cache) {
        return Ok(());
    }
    cache.version = CACHE_VERSION;
    cache.updated_at = now_secs();
    prune_oldest(&mut cache.models, MAX_ENTRIES, |entry| entry.checked_at);
    prune_oldest(&mut cache.refused_accounts, MAX_ACCOUNT_REFUSALS, |entry| {
        entry.checked_at
    });
    write_private(&path, &serde_json::to_vec(&cache)?)?;
    // Refresh in-memory stamp immediately when readable.
    if let Some(stamp) = file_stamp(&path) {
        *PARSED.lock().unwrap_or_else(|e| e.into_inner()) = Some(((path, stamp), Arc::new(cache)));
    }
    Ok(())
}

fn prune_oldest<V>(map: &mut BTreeMap<String, V>, limit: usize, ts: impl Fn(&V) -> u64) {
    while map.len() > limit {
        let victim = map
            .iter()
            .min_by_key(|(_, value)| ts(value))
            .map(|(key, _)| key.clone());
        match victim {
            Some(key) => {
                map.remove(&key);
            }
            None => break,
        }
    }
}

fn normalized_base_url(selection: &ModelSelection) -> Option<String> {
    if let Some(url) = selection.base_url.as_deref() {
        return Some(url.trim_end_matches('/').to_owned());
    }
    if selection.provider == "mlx" {
        return None;
    }
    if selection.provider == chatgpt::PROVIDER {
        return Some(chatgpt::BASE_URL.trim_end_matches('/').to_owned());
    }
    bundled::builtin_provider(&selection.provider)
        .map(|p| p.base_url.trim_end_matches('/').to_owned())
}

fn effective_account_id(user_dir: &Path, selection: &ModelSelection) -> Option<String> {
    if let Some(id) = selection.account_id.as_deref().filter(|s| !s.is_empty()) {
        return Some(id.to_owned());
    }
    if selection.provider == chatgpt::PROVIDER {
        let dir = subscription_auth_dir(user_dir).ok()?;
        return chatgpt::active_account_id(&dir);
    }
    None
}

fn model_key(user_dir: &Path, selection: &ModelSelection) -> String {
    let base = normalized_base_url(selection);
    let account = effective_account_id(user_dir, selection);
    let protocol = selection
        .protocol
        .unwrap_or_else(|| ApiProtocol::builtin(&selection.provider));
    let scope = serde_json::json!([
        "dial-image-capability-v1",
        selection.provider,
        base,
        protocol,
        account,
        selection.model
    ]);
    format!("{:x}", Sha256::digest(scope.to_string().as_bytes()))
}

fn account_key(provider: &str, account_id: &str) -> String {
    let scope = serde_json::json!(["dial-image-account-v1", provider, account_id]);
    format!("{:x}", Sha256::digest(scope.to_string().as_bytes()))
}

/// Look up the cached image-generation capability of `selection` (offline, stamp-cached).
pub fn lookup(user_dir: &Path, selection: &ModelSelection) -> Option<CachedImageCapability> {
    let cache = load_cached(user_dir);
    if let Some(account) = effective_account_id(user_dir, selection)
        && let Some(refusal) = cache
            .refused_accounts
            .get(&account_key(&selection.provider, &account))
    {
        return Some(CachedImageCapability {
            provider: selection.provider.clone(),
            model: selection.model.clone(),
            status: ImageCapabilityStatus::Incapable,
            source: ImageCapabilitySource::Verified,
            api: Some(ImageGenerationApi::Disabled),
            detail: refusal.detail.clone(),
            checked_at: refusal.checked_at,
        });
    }
    cache.models.get(&model_key(user_dir, selection)).cloned()
}

/// Whether `account_id` on `provider` has a persisted account-level hosted-image refusal.
pub fn account_refused(user_dir: &Path, provider: &str, account_id: &str) -> bool {
    load_cached(user_dir)
        .refused_accounts
        .contains_key(&account_key(provider, account_id))
}

/// Persist that `selection` successfully generated an image using `api`.
pub fn record_verified(
    user_dir: &Path,
    selection: &ModelSelection,
    api: ImageGenerationApi,
) -> Result<()> {
    if api == ImageGenerationApi::Disabled
        || selection.provider.is_empty()
        || selection.model.is_empty()
    {
        return Ok(());
    }
    let key = model_key(user_dir, selection);
    let acct =
        effective_account_id(user_dir, selection).map(|id| account_key(&selection.provider, &id));
    let checked_at = now_secs();
    mutate_cache(user_dir, |cache| {
        if let Some(acct_key) = acct {
            cache.refused_accounts.remove(&acct_key);
        }
        cache.models.insert(
            key,
            CachedImageCapability {
                provider: selection.provider.clone(),
                model: selection.model.clone(),
                status: ImageCapabilityStatus::Capable,
                source: ImageCapabilitySource::Verified,
                api: Some(api),
                detail: None,
                checked_at,
            },
        );
        true
    })
}

/// Persist that `selection` definitively refused image generation.
pub fn record_incapable(
    user_dir: &Path,
    selection: &ModelSelection,
    detail: impl Into<String>,
) -> Result<()> {
    if selection.provider.is_empty() || selection.model.is_empty() {
        return Ok(());
    }
    let key = model_key(user_dir, selection);
    let checked_at = now_secs();
    let detail = detail.into();
    mutate_cache(user_dir, |cache| {
        cache.models.insert(
            key,
            CachedImageCapability {
                provider: selection.provider.clone(),
                model: selection.model.clone(),
                status: ImageCapabilityStatus::Incapable,
                source: ImageCapabilitySource::Verified,
                api: Some(ImageGenerationApi::Disabled),
                detail: (!detail.is_empty()).then_some(detail),
                checked_at,
            },
        );
        true
    })
}

/// Persist an account-wide hosted-image refusal (for example, a ChatGPT account lacking the
/// hosted `image_generation` tool).
pub fn record_account_refused(
    user_dir: &Path,
    provider: &str,
    account_id: &str,
    detail: impl Into<String>,
) -> Result<()> {
    if provider.is_empty() || account_id.is_empty() {
        return Ok(());
    }
    let key = account_key(provider, account_id);
    let checked_at = now_secs();
    let detail = detail.into();
    mutate_cache(user_dir, |cache| {
        cache.refused_accounts.insert(
            key,
            AccountRefusal {
                provider: provider.to_owned(),
                detail: (!detail.is_empty()).then_some(detail),
                checked_at,
            },
        );
        true
    })
}

/// Clear persisted verified refusals (account-wide and per-model) so a user-requested re-check
/// can test models/accounts again (e.g. after a subscription upgrade).
pub fn clear_refusals(user_dir: &Path) -> Result<()> {
    mutate_cache(user_dir, |cache| {
        let had_accounts = !cache.refused_accounts.is_empty();
        cache.refused_accounts.clear();
        let before = cache.models.len();
        cache
            .models
            .retain(|_, rec| rec.status != ImageCapabilityStatus::Incapable);
        had_accounts || cache.models.len() != before
    })
}

/// Discovery-level image capability of one model: status, its source and the generation API.
type Classification = (
    ImageCapabilityStatus,
    ImageCapabilitySource,
    Option<ImageGenerationApi>,
);

/// Classify a discovered model entry into its discovery-level image capability, if eligible.
fn classify_discovered(entry: &ModelEntry) -> Option<Classification> {
    if !entry.available || entry.metadata.image_generation == Some(ImageGenerationApi::Disabled) {
        return None;
    }
    if let Some(api) = entry.metadata.image_generation {
        let documented = entry.selection.base_url.as_deref().and_then(|url| {
            documented_image_generation(&entry.selection.provider, url, &entry.selection.model)
        }) == Some(api);
        let source = if documented {
            ImageCapabilitySource::Documented
        } else {
            ImageCapabilitySource::Metadata
        };
        return Some((ImageCapabilityStatus::Capable, source, Some(api)));
    }
    if entry.metadata.kind == ModelKind::Image {
        return Some((
            ImageCapabilityStatus::Capable,
            ImageCapabilitySource::Metadata,
            None,
        ));
    }
    if entry.accepts_image_role() {
        return Some((
            ImageCapabilityStatus::Candidate,
            ImageCapabilitySource::Candidate,
            None,
        ));
    }
    None
}

/// Update the capability cache from a fresh discovery pass, preserving verified runtime outcomes.
pub fn update_from_discovery(user_dir: &Path, entries: &[ModelEntry]) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let checked_at = now_secs();
    let classified: Vec<(String, &ModelEntry, Option<Classification>)> = entries
        .iter()
        .filter(|entry| !entry.selection.provider.is_empty() && !entry.selection.model.is_empty())
        .map(|entry| {
            (
                model_key(user_dir, &entry.selection),
                entry,
                classify_discovered(entry),
            )
        })
        .collect();
    mutate_cache(user_dir, |cache| {
        let mut changed = false;
        for (key, entry, classification) in classified {
            if cache
                .models
                .get(&key)
                .is_some_and(|existing| existing.source == ImageCapabilitySource::Verified)
            {
                continue;
            }
            match classification {
                Some((status, source, api)) => {
                    let next = CachedImageCapability {
                        provider: entry.selection.provider.clone(),
                        model: entry.selection.model.clone(),
                        status,
                        source,
                        api,
                        detail: None,
                        checked_at,
                    };
                    cache.models.insert(key, next);
                    changed = true;
                }
                None => {
                    if cache.models.remove(&key).is_some() {
                        changed = true;
                    }
                }
            }
        }
        changed
    })
}

/// Apply persisted verified capabilities and refusals onto `entries` (offline, stamp-cached).
pub fn apply_cached(user_dir: &Path, entries: &mut [ModelEntry]) {
    let cache = load_cached(user_dir);
    if cache.models.is_empty() && cache.refused_accounts.is_empty() {
        return;
    }
    for entry in entries {
        if let Some(account) = effective_account_id(user_dir, &entry.selection)
            && cache
                .refused_accounts
                .contains_key(&account_key(&entry.selection.provider, &account))
            && entry.selection.provider == chatgpt::PROVIDER
        {
            entry.metadata.image_generation = Some(ImageGenerationApi::Disabled);
            continue;
        }
        let key = model_key(user_dir, &entry.selection);
        let Some(rec) = cache.models.get(&key) else {
            continue;
        };
        if rec.source != ImageCapabilitySource::Verified {
            continue;
        }
        match rec.status {
            ImageCapabilityStatus::Capable => {
                if entry.metadata.image_generation != Some(ImageGenerationApi::Disabled)
                    && let Some(api) = rec.api
                {
                    entry.metadata.image_generation = Some(api);
                }
            }
            ImageCapabilityStatus::Incapable => {
                entry.metadata.image_generation = Some(ImageGenerationApi::Disabled);
            }
            ImageCapabilityStatus::Candidate => {}
        }
    }
}

/// Summary timestamp of when the capability cache was last updated (unix seconds, 0 if never).
pub fn last_updated_at(user_dir: &Path) -> u64 {
    load_cached(user_dir).updated_at
}

/// Every cached capable or candidate image model, sorted by provider then model.
pub fn cached_capable(user_dir: &Path) -> Vec<CachedImageCapability> {
    let cache = load_cached(user_dir);
    let mut rows: Vec<CachedImageCapability> = cache
        .models
        .values()
        .filter(|rec| rec.status != ImageCapabilityStatus::Incapable)
        .cloned()
        .collect();
    rows.sort_by(|a, b| {
        a.provider
            .cmp(&b.provider)
            .then_with(|| a.model.cmp(&b.model))
    });
    rows
}

#[cfg(test)]
mod tests;
