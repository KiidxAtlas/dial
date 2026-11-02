//! Remote-attempt authentication and recovery, in the same order as dispatch: reasoning compatibility, one token
//! refresh, key/account failover, auth diagnostics, then policy-controlled cancellable backoff.
use super::{ApiProtocol, Backend, ProviderKind, llm};
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeSet,
    sync::{Arc, atomic::AtomicBool},
    time::Instant,
};

pub(super) struct RetryState {
    pub profile: Option<llm::profiles::ModelProfile>,
    pub reasoning_stripped: bool,
    pub oauth_retry: OAuthRetry,
    pub active_account: Option<String>,
    pub tried_accounts: BTreeSet<String>,
}

pub(super) struct FailedAttempt<'a> {
    pub error: anyhow::Error,
    pub req: &'a llm::Chat,
    pub url: &'a str,
    pub model: &'a str,
    pub attempt: usize,
    pub retry_policy: &'a llm::retry::Policy,
    pub attempt_started: Instant,
    pub interrupt: &'a Arc<AtomicBool>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum OAuthRetry {
    NotRequested,
    Pending,
    Refreshed,
}

pub(super) struct AuthAttempt<'a> {
    pub attempt: usize,
    pub interrupt: &'a Arc<AtomicBool>,
    pub fallback_auth: Option<&'a str>,
    pub fallback_url: &'a str,
}

pub(super) struct PreparedAuth<'a> {
    fallback_auth: Option<&'a str>,
    refreshed_access: Option<llm::auth::SecretToken>,
    retry_endpoint_key: Option<String>,
    pub oauth_access: Option<llm::oauth::Access>,
    pub url: Option<String>,
}

impl PreparedAuth<'_> {
    pub(super) fn token(&self) -> Option<&str> {
        self.oauth_access
            .as_ref()
            .map(|access| access.token.as_str())
            .or_else(|| self.refreshed_access.as_ref().map(|token| token.as_str()))
            .or(self.retry_endpoint_key.as_deref())
            .or(self.fallback_auth)
    }
}

enum AccountFailover {
    Unavailable,
    IdentityConflict,
    Rebound,
}

impl Backend {
    pub(super) fn prepare_remote_auth<'a>(
        &mut self,
        state: &mut RetryState,
        attempt: AuthAttempt<'a>,
        on_notice: &mut dyn FnMut(&str),
    ) -> Result<PreparedAuth<'a>> {
        let AuthAttempt {
            attempt,
            interrupt,
            fallback_auth,
            fallback_url,
        } = attempt;
        let refreshed_access = if attempt > 1 {
            match &self.kind {
                ProviderKind::ChatGpt { user_dir, .. } => Some(
                    llm::chatgpt::scoped_account_access(
                        user_dir,
                        state
                            .active_account
                            .as_deref()
                            .context("ChatGPT backend has no pinned account")?,
                        Arc::clone(interrupt),
                    )?
                    .1,
                ),
                // Claude subscription tokens expire within hours and a retry can wait minutes:
                // read the stored sign-in again (it refreshes near expiry) rather than resending
                // the first attempt's token.
                ProviderKind::AnthropicSubscription { user_dir, .. } => Some(
                    llm::anthropic_oauth::access_token(user_dir, Arc::clone(interrupt))?,
                ),
                _ => None,
            }
        } else {
            None
        };
        // Keep first-dispatch preparation unchanged, but never reuse a key across backoff:
        // removal must resolve to no auth, not fall back to the previous attempt's key.
        let retry_endpoint_key = if attempt > 1 {
            self.endpoint_api_key()?
        } else {
            None
        };
        let oauth_access = match &self.oauth {
            Some(binding) if state.oauth_retry == OAuthRetry::Pending => {
                state.oauth_retry = OAuthRetry::Refreshed;
                Some(binding.refreshed_access(interrupt)?)
            }
            Some(binding) => Some(binding.access(interrupt)?),
            None => None,
        };
        let prepared = PreparedAuth {
            fallback_auth: if attempt > 1
                && matches!(
                    self.kind,
                    ProviderKind::Endpoint { .. } | ProviderKind::AnthropicEndpoint { .. }
                ) {
                None
            } else {
                fallback_auth
            },
            refreshed_access,
            retry_endpoint_key,
            oauth_access,
            url: None,
        };
        let auth = prepared.token();
        if !self.subscription_noticed
            && auth.is_some_and(llm::anthropic::subscription_token)
            && matches!(
                self.kind,
                ProviderKind::AnthropicEndpoint { .. } | ProviderKind::AnthropicSubscription { .. }
            )
        {
            on_notice(llm::anthropic::SUBSCRIPTION_WARNING);
            self.subscription_noticed = true;
        }
        let oauth_url = match prepared
            .oauth_access
            .as_ref()
            .and_then(|access| access.base_url.as_deref())
        {
            Some(base) => Some(llm::transports::route_url(
                self.protocol.unwrap_or(ApiProtocol::ChatCompletions),
                base,
                self.allow_insecure,
            )?),
            None => None,
        };
        let url = oauth_url.as_deref().unwrap_or(fallback_url);
        if let Some(binding) = &self.oauth {
            binding.ensure_route(
                oauth_url.is_some(),
                url,
                self.protocol.unwrap_or(ApiProtocol::ChatCompletions),
            )?;
        }
        Ok(PreparedAuth {
            url: oauth_url,
            ..prepared
        })
    }

    pub(super) fn recover_remote_attempt(
        &mut self,
        state: &mut RetryState,
        failure: FailedAttempt<'_>,
        on_notice: &mut dyn FnMut(&str),
        on_event: &mut dyn FnMut(&llm::StreamEvent) -> bool,
    ) -> Result<()> {
        let FailedAttempt {
            error,
            req,
            url,
            model,
            attempt,
            retry_policy,
            attempt_started,
            interrupt,
        } = failure;
        if !state.reasoning_stripped
            && attempt < retry_policy.max_attempts
            && llm::reasoning_memory::rejects_reasoning(&error)
            && state
                .profile
                .as_ref()
                .is_some_and(|p| p.reasoning_effort.is_some() || p.thinking == Some(false))
            && let Some(current) = state.profile.take()
        {
            state.profile = Some(llm::reasoning_memory::without_reasoning(current));
            state.reasoning_stripped = true;
            on_notice(
                "Provider rejected the reasoning parameter; retrying without it and remembering this for the session",
            );
            return Ok(());
        }
        if self.oauth.is_some()
            && state.oauth_retry == OAuthRetry::NotRequested
            && attempt < retry_policy.max_attempts
            && error
                .downcast_ref::<llm::HttpError>()
                .is_some_and(|http| matches!(http.status, 401 | 403))
        {
            state.oauth_retry = OAuthRetry::Pending;
            on_notice(
                "Provider rejected the OAuth access token; refreshing the sign-in and retrying",
            );
            return Ok(());
        }
        let class = llm::errors::classify(&error);
        if attempt < retry_policy.max_attempts
            && let Some(cooldown) = llm::key_pool::cooldown(
                &class,
                error
                    .downcast_ref::<llm::HttpError>()
                    .and_then(|http| http.retry_after),
                std::time::SystemTime::now(),
            )
            && let (Some(pool), Some(source)) = (self.key_pool.as_mut(), self.credential.as_ref())
            && pool
                .rotate(
                    llm::key_pool::process_blocks(),
                    &source.base_url,
                    Instant::now(),
                    cooldown,
                    |variable| source.has_key(variable),
                )
                .is_some()
        {
            on_notice(
                "API key was rate limited or out of quota; retrying with the next configured key",
            );
            return Ok(());
        }
        // OAuth accounts: a rate limit, quota exhaustion, or rejected credential cools that
        // account down and moves this session to the next stored one. ChatGPT keeps its own
        // failover below, with its session-binding guards.
        if attempt < retry_policy.max_attempts
            && retry_policy.rotate_accounts
            && !matches!(self.kind, ProviderKind::ChatGpt { .. })
            && let Some(binding) = self.oauth.as_mut()
            && binding.rotate(
                &class,
                error
                    .downcast_ref::<llm::HttpError>()
                    .and_then(|http| http.retry_after),
            )
        {
            on_notice(
                "OAuth account was rate limited, out of quota, or rejected; retrying with another signed-in account",
            );
            return Ok(());
        }
        if attempt < retry_policy.max_attempts
            && retry_policy.rotate_accounts
            && matches!(
                class,
                llm::errors::ErrorClass::Auth | llm::errors::ErrorClass::QuotaExhausted { .. }
            )
        {
            match self.failover_chatgpt_account(state, req, model)? {
                AccountFailover::Unavailable => {}
                AccountFailover::IdentityConflict => return Err(error.context(
                    "ChatGPT account failover refused: this request has a session/cache identity or native replay history; explicitly select another allowed account and start a clean chat (or rebind portable history) before retrying",
                )),
                AccountFailover::Rebound => {
                    on_notice("ChatGPT credential was rejected or quota exhausted; retrying with another signed-in account");
                    return Ok(());
                }
            }
        }
        if let Some(status @ (401 | 403)) = error
            .downcast_ref::<llm::HttpError>()
            .filter(|_| matches!(class, llm::errors::ErrorClass::Auth))
            .map(|http| http.status)
        {
            let host = llm::host_of(url);
            let key = match host.as_deref() {
                Some("api.openai.com") => "OPENAI_API_KEY",
                Some("api.anthropic.com") => "ANTHROPIC_API_KEY",
                Some("openrouter.ai") => "OPENROUTER_API_KEY",
                Some("api.groq.com") => "GROQ_API_KEY",
                Some("api.mistral.ai") => "MISTRAL_API_KEY",
                Some("api.deepseek.com") => "DEEPSEEK_API_KEY",
                _ => "the endpoint's configured API key",
            };
            return Err(error.context(format!("provider authentication failed (HTTP {status}); check {key} or sign in through /providers")));
        }
        let class = llm::errors::classify(&error);
        let hinted = error
            .downcast_ref::<llm::HttpError>()
            .and_then(|http| http.retry_after);
        // A failure before any response (reset, refused, broken pipe) retries promptly once.
        let connection = error.chain().any(|cause| {
            cause
                .downcast_ref::<llm::TransportError>()
                .is_some_and(|transport| transport.connection)
        });
        let delay = match llm::retry::decide(&class, hinted, attempt, connection, retry_policy) {
            llm::retry::Decision::Retry { delay, reason } => {
                if error
                    .downcast_ref::<llm::stream::IncompleteStream>()
                    .is_some()
                    || error
                        .downcast_ref::<llm::anthropic::IncompleteStream>()
                        .is_some()
                {
                    on_notice(
                        "Provider stream disconnected or was incomplete; discarding the partial reply and retrying. No partial tool calls will execute.",
                    );
                }
                (delay, reason)
            }
            llm::retry::Decision::Stop(Some(message)) => return Err(error.context(message)),
            llm::retry::Decision::Stop(None) => return Err(error),
        };
        let (delay, reason) = delay;
        let cause = format!("{error:#}");
        let cause = cause.lines().next().unwrap_or("").trim();
        let cause: String = if cause.chars().count() > 160 {
            cause
                .chars()
                .take(159)
                .chain(std::iter::once('…'))
                .collect()
        } else {
            cause.to_owned()
        };
        // How long the failed attempt ran, so a full silence limit and a quick refusal read differently.
        let cause = format!(
            "{cause} (after {:.1}s)",
            attempt_started.elapsed().as_secs_f64()
        );
        on_notice(&format!(
            "{reason}: {cause}; retrying in {:.1}s (attempt {}/{})",
            delay.as_secs_f64(),
            attempt + 1,
            retry_policy.attempts(&class)
        ));
        let pending = self.retry_control.begin_wait();
        if !on_event(&llm::StreamEvent::Retry { waiting: true }) {
            return Err(llm::Interrupted.into());
        }
        pending.wait(delay, interrupt)?;
        Ok(())
    }
    fn failover_chatgpt_account(
        &mut self,
        state: &mut RetryState,
        req: &llm::Chat,
        model: &str,
    ) -> Result<AccountFailover> {
        let ProviderKind::ChatGpt { user_dir, .. } = &self.kind else {
            return Ok(AccountFailover::Unavailable);
        };
        let allowed = self
            .steering_config
            .as_ref()
            .and_then(|config| config.chatgpt_allowed_accounts.as_ref());
        if let Some(candidate) = llm::chatgpt::accounts(user_dir)?
            .into_iter()
            .find(|account| {
                account.signed_in
                    && !state.tried_accounts.contains(&account.client_id)
                    && allowed.is_none_or(|set| set.contains(&account.client_id))
            })
        {
            // A request does not own the persistent session/role binding. Changing only
            // its auth would stamp B-owned history into an A-bound conversation. An
            // operator must explicitly select/rebind a clean session instead.
            if self.session_bound
                || self.prompt_cache_key.is_some()
                || llm::responses::has_native_history(req)
            {
                return Ok(AccountFailover::IdentityConflict);
            }
            // The request was prepared using this profile. Failover must not silently
            // change capabilities, image handling or token budgets halfway through it.
            if self
                .steering_config
                .as_ref()
                .and_then(|config| config.active_model_profile())
                .is_none()
            {
                let candidate_profile = llm::chatgpt::scoped_model_profile(
                    user_dir,
                    Some(&candidate.client_id),
                    model,
                )?;
                ensure!(
                    serde_json::to_value(&candidate_profile)?
                        == serde_json::to_value(&self.profile)?,
                    "ChatGPT account failover requires a different model profile; explicitly select the account and rebuild the backend before retrying"
                );
            }
            state.tried_accounts.insert(candidate.client_id.clone());
            state.active_account = Some(candidate.client_id);
            // Unbound portable requests have no external identity to reconcile. Pin
            // the backend as well as this attempt, even if the next attempt fails:
            // the next request must not revert to the rejected account.
            self.account_id = state.active_account.clone();
            return Ok(AccountFailover::Rebound);
        }
        Ok(AccountFailover::Unavailable)
    }
}
