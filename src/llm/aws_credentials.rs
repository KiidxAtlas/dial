//! AWS SDK credential-chain access and SigV4 signing for Bedrock.
//!
//! With the `aws` feature (default) this resolves through the SDK provider chain and signs
//! requests. Without it, SigV4 is unavailable and resolution fails with a clear error; bearer-key
//! Bedrock never reaches this module's signing.
#[cfg(not(feature = "aws"))]
pub(super) use disabled::{Credentials, resolve};
#[cfg(feature = "aws")]
pub(super) use enabled::{Credentials, resolve};

#[cfg(feature = "aws")]
mod enabled {
    use anyhow::{Context, Result, anyhow};
    use aws_config::{BehaviorVersion, Region};
    use aws_credential_types::provider::ProvideCredentials;
    use std::{
        collections::HashMap,
        sync::{
            Arc, LazyLock, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, SystemTime},
    };

    /// Keep SDK providers alive so their own refresh/cache semantics span requests.
    static CONFIGS: LazyLock<Mutex<HashMap<String, aws_config::SdkConfig>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    /// Retain the SDK's shared secret storage instead of copying credentials for each request.
    pub(in crate::llm) struct Credentials(aws_credential_types::Credentials);

    impl Credentials {
        pub(in crate::llm) fn account_id(&self) -> Option<&str> {
            self.0.account_id().map(|id| id.as_str())
        }

        pub(in crate::llm) fn access_key_id(&self) -> &str {
            self.0.access_key_id()
        }

        /// SigV4-signs a Bedrock `POST` and returns the headers to add to the request.
        pub(in crate::llm) fn sign(
            &self,
            region: &str,
            url: &str,
            headers: &[(&str, &str)],
            body: &[u8],
        ) -> Result<Vec<(String, String)>> {
            use aws_sigv4::http_request::{
                PayloadChecksumKind, SignableBody, SignableRequest, SigningSettings, sign,
            };
            let identity = self.0.clone().into();
            let mut settings = SigningSettings::default();
            settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
            let params = aws_sigv4::sign::v4::SigningParams::builder()
                .identity(&identity)
                .region(region)
                .name("bedrock")
                .time(SystemTime::now())
                .settings(settings)
                .build()?
                .into();
            let signable = SignableRequest::new(
                "POST",
                url,
                headers.iter().copied(),
                SignableBody::Bytes(body),
            )?;
            let (instructions, _) = sign(signable, &params)?.into_parts();
            Ok(instructions
                .headers()
                .map(|(name, value)| (name.to_owned(), value.to_owned()))
                .collect())
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
    pub(in crate::llm) fn resolve(
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
                            .http_client(
                                // The SDK's default HTTPS client is rustls on aws-lc-rs, a large C
                                // build; ring is already in the graph and serves the same purpose.
                                aws_smithy_http_client::Builder::new()
                                    .tls_provider(aws_smithy_http_client::tls::Provider::rustls(
                                        aws_smithy_http_client::tls::rustls_provider::CryptoMode::Ring,
                                    ))
                                    .build_https(),
                            )
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
                    return Err(super::super::Interrupted.into());
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

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn aws_signing_preserves_session_token_and_payload_checksum() {
            let credentials = Credentials(aws_credential_types::Credentials::new(
                "AKIDEXAMPLE",
                "test-secret",
                Some("test-session".to_owned()),
                None,
                "dial-test",
            ));
            let headers = credentials
                .sign(
                    "us-east-1",
                    "https://bedrock-runtime.us-east-1.amazonaws.com/model/example/converse-stream",
                    &[("content-type", "application/json")],
                    b"{}",
                )
                .unwrap();
            let headers: HashMap<_, _> = headers.into_iter().collect();
            assert_eq!(headers["x-amz-security-token"], "test-session");
            assert_eq!(
                headers["x-amz-content-sha256"],
                "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
            );
            assert!(headers["authorization"].contains("Credential=AKIDEXAMPLE/"));
            assert!(headers["authorization"].contains("/us-east-1/bedrock/aws4_request"));
            let debug = format!("{credentials:?}");
            assert!(!debug.contains("test-secret"));
            assert!(!debug.contains("test-session"));
        }
    }
}

/// Built without the `aws` feature: no credentials can exist, so the methods are unreachable and
/// resolution reports why SigV4 is unavailable.
#[cfg(not(feature = "aws"))]
mod disabled {
    use anyhow::{Result, bail};
    use std::sync::{Arc, atomic::AtomicBool};

    pub(in crate::llm) enum Credentials {}

    impl Credentials {
        pub(in crate::llm) fn account_id(&self) -> Option<&str> {
            match *self {}
        }

        pub(in crate::llm) fn access_key_id(&self) -> &str {
            match *self {}
        }

        pub(in crate::llm) fn sign(
            &self,
            _region: &str,
            _url: &str,
            _headers: &[(&str, &str)],
            _body: &[u8],
        ) -> Result<Vec<(String, String)>> {
            match *self {}
        }
    }

    pub(in crate::llm) fn resolve(
        _region: &str,
        _timeout: u64,
        _interrupt: Arc<AtomicBool>,
    ) -> Result<Credentials> {
        bail!(
            "Bedrock SigV4 signing needs the `aws` feature (AWS credential chain); set a Bedrock bearer key or build with default features"
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn slim_aws_credentials_fail_explicitly_without_network_access() {
            let error = match resolve("us-east-1", 1, Arc::new(AtomicBool::new(false))) {
                Ok(_) => panic!("credentials cannot exist without the aws feature"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("`aws` feature"));
        }
    }
}
