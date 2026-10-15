//! AWS SDK credential-chain access for Bedrock.
use anyhow::{Context, Result, anyhow};
use aws_config::{BehaviorVersion, Region};
use aws_credential_types::provider::ProvideCredentials;
use std::{
    collections::HashMap,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// Keep SDK providers alive so their own refresh/cache semantics span requests.
static CONFIGS: LazyLock<Mutex<HashMap<String, aws_config::SdkConfig>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Retain the SDK's shared secret storage instead of copying credentials for each request.
pub(super) struct Credentials(aws_credential_types::Credentials);
impl Credentials {
    pub(super) fn sdk(&self) -> &aws_credential_types::Credentials {
        &self.0
    }
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &"[REDACTED]")
            .field("secret_access_key", &"[REDACTED]")
            .field(
                "session_token",
                &self.0.session_token().map(|_| "[REDACTED]"),
            )
            .field("expires_at", &self.0.expiry())
            .finish()
    }
}

static RUNTIME: LazyLock<std::result::Result<tokio::runtime::Runtime, String>> =
    LazyLock::new(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|error| error.to_string())
    });

fn runtime() -> Result<&'static tokio::runtime::Runtime> {
    RUNTIME
        .as_ref()
        .map_err(|error| anyhow!("failed to initialize AWS credential runtime: {error}"))
}

/// Resolve using the standard AWS SDK provider chain (environment, profiles, SSO,
/// process, web identity, container and instance metadata providers).
pub(super) fn resolve(
    region: &str,
    timeout: u64,
    interrupt: Arc<AtomicBool>,
) -> Result<Credentials> {
    let region = region.to_owned();
    let config_key = format!(
        "{}|{}|{}|{}|{}",
        region,
        std::env::var("AWS_PROFILE").unwrap_or_default(),
        std::env::var("AWS_CONFIG_FILE").unwrap_or_default(),
        std::env::var("AWS_SHARED_CREDENTIALS_FILE").unwrap_or_default(),
        std::env::var("AWS_SDK_LOAD_CONFIG").unwrap_or_default()
    );
    let duration = Duration::from_secs(timeout);
    runtime()?.block_on(async move {
        let future = async move {
            let cached = CONFIGS
                .lock()
                .map_err(|_| anyhow!("AWS credential configuration cache unavailable"))?
                .get(&config_key)
                .cloned();
            let config = match cached {
                Some(config) => config,
                None => {
                    let config = aws_config::defaults(BehaviorVersion::latest())
                        .region(Region::new(region))
                        .load()
                        .await;
                    CONFIGS
                        .lock()
                        .map_err(|_| anyhow!("AWS credential configuration cache unavailable"))?
                        .entry(config_key)
                        .or_insert_with(|| config.clone());
                    config
                }
            };
            let provider = config
                .credentials_provider()
                .ok_or_else(|| anyhow!("AWS credential provider chain is unavailable"))?;
            provider
                .provide_credentials()
                .await
                .context("AWS credential resolution failed")
        };
        tokio::pin!(future);
        let deadline = async {
            if duration.is_zero() {
                std::future::pending::<()>().await;
            } else {
                tokio::time::sleep(duration).await;
            }
        };
        tokio::pin!(deadline);
        loop {
            if interrupt.load(Ordering::SeqCst) {
                return Err(super::Interrupted.into());
            }
            tokio::select! {
                biased;
                _ = &mut deadline => return Err(anyhow!("AWS credential resolution timed out")),
                result = &mut future => return result.map(Credentials),
                _ = tokio::time::sleep(Duration::from_millis(20)) => {}
            }
        }
    })
}
