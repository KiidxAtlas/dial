//! Model backend construction: the resident `Runtime`, provider backends and session-scoped auth and memory authority.

use crate::agent::*;

/// Holds the model backend between runs so a local server stays resident.
pub struct Runtime {
    pub(in crate::agent) backend: Option<Backend>,
}

impl Runtime {
    pub fn new() -> Self {
        Self { backend: None }
    }

    pub fn backend(&mut self, config: &Config) -> Result<&mut Backend> {
        if self.backend.is_none() {
            self.backend = Some(make_backend(config)?);
        }
        self.backend.as_mut().context("backend missing")
    }
    pub fn usage(&mut self, config: &Config) -> Result<llm::usage::Groups> {
        Ok(self.backend(config)?.usage_scope_snapshot())
    }

    /// Move the resident backend out; a later `backend()` rebuilds it on demand.
    pub fn take_backend(&mut self, config: &Config) -> Result<Backend> {
        if let Some(backend) = self.backend.take() {
            return Ok(backend);
        }
        make_backend(config)
    }

    pub fn release(&mut self) {
        if let Some(backend) = self.backend.as_mut() {
            backend.release();
        }
        self.backend = None;
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

pub fn make_backend(config: &Config) -> Result<Backend> {
    config.require_model()?;
    let requested_account = config
        .endpoint
        .as_ref()
        .and_then(|e| e.account_id.as_deref());
    ensure!(
        requested_account.is_none() || config.provider.as_deref() == Some("chatgpt"),
        "account_id is supported only for ChatGPT; Anthropic has one account"
    );
    let account_id = if config.provider.as_deref() == Some("chatgpt") {
        if let Some(id) = requested_account {
            ensure!(
                config.chatgpt_pinned_accounts.contains(id),
                "ChatGPT account_id is not authorized by private global policy"
            );
        }
        let id = llm::chatgpt::resolve_account(&config.user_dir, requested_account)?;
        ensure!(
            config
                .chatgpt_allowed_accounts
                .as_ref()
                .is_none_or(|ids| ids.contains(&id)),
            "ChatGPT account is restricted by private global policy"
        );
        Some(id)
    } else {
        None
    };
    let oauth = config
        .provider
        .as_deref()
        .and_then(|provider| llm::oauth::Binding::for_provider(&config.user_dir, provider));
    let kind = match (&config.provider, &config.endpoint) {
        (Some(provider), _) if provider == crate::llm::chatgpt::PROVIDER => {
            if let Some(endpoint) = &config.endpoint {
                ensure!(
                    endpoint.base_url == crate::llm::chatgpt::BASE_URL
                        && endpoint.api_key_env.is_none()
                        && endpoint
                            .protocol
                            .is_none_or(|p| p == llm::model_config::ApiProtocol::Responses),
                    "ChatGPT subscription uses its official Responses endpoint and OAuth credentials only"
                );
            }
            ProviderKind::ChatGpt {
                model: config.model.clone(),
                user_dir: config.user_dir.clone(),
            }
        }
        (Some(provider), Some(endpoint))
            if provider == "anthropic"
                && endpoint.base_url == crate::llm::anthropic::BASE_URL
                && crate::llm::anthropic_oauth::signed_in(&config.user_dir) =>
        {
            ensure!(
                endpoint
                    .protocol
                    .is_none_or(|p| p == llm::model_config::ApiProtocol::Anthropic),
                "Claude subscription requires the Anthropic protocol"
            );
            ProviderKind::AnthropicSubscription {
                model: config.model.clone(),
                user_dir: config.user_dir.clone(),
            }
        }
        (Some(provider), Some(endpoint)) if provider == "anthropic" => {
            ProviderKind::AnthropicEndpoint {
                base_url: endpoint.base_url.clone(),
                model: config.model.clone(),
                api_key: crate::llm::catalog::resolve_credential(
                    &config.user_dir,
                    &endpoint.base_url,
                    endpoint.api_key_env.as_deref(),
                )?,
            }
        }
        (Some(provider), _) if provider == crate::llm::apple::PROVIDER => ProviderKind::Apple,
        (_, None) => ProviderKind::Local {
            model_dir: crate::llm::download::ensure_local_model(&config.python, &config.model)?,
        },
        (_, Some(endpoint)) => ProviderKind::Endpoint {
            base_url: endpoint.base_url.clone(),
            model: config.model.clone(),
            // A stored provider OAuth grant authenticates per request instead of an API key.
            api_key: if oauth.is_some() {
                None
            } else {
                crate::llm::catalog::resolve_credential(
                    &config.user_dir,
                    &endpoint.base_url,
                    endpoint.api_key_env.as_deref(),
                )?
            },
        },
    };
    let layout = Layout {
        project_root: config.project_root.clone(),
        project_dir: config.project_root.join(".dial"),
        user_dir: config.user_dir.clone(),
        state_dir: config.state_dir.clone(),
    };
    let mut metadata_selection = config.selection();
    if metadata_selection.headers.is_none() {
        metadata_selection.headers = Some(config.request_headers().to_vec());
    }
    let metadata = llm::catalog::model_metadata(&layout, &metadata_selection);
    let mut backend = Backend::new(
        kind,
        config.python.clone(),
        config.worker_timeout_secs.min(600),
    )
    .with_remote_idle_timeout(config.remote_model_idle_timeout_secs)
    .with_codex_websocket(config.codex_websocket)
    .with_protocol(
        config
            .endpoint
            .as_ref()
            .and_then(|e| e.protocol)
            .or_else(|| oauth.as_ref().map(|binding| binding.flow().protocol))
            .or_else(|| {
                config
                    .provider
                    .as_deref()
                    .map(llm::model_config::ApiProtocol::builtin)
            }),
    )
    .with_oauth(oauth)
    .with_headers(metadata_selection.headers.take().unwrap_or_default())?
    .with_allow_insecure(config.endpoint.as_ref().is_some_and(|e| e.allow_insecure))
    .with_metadata(metadata)
    .with_steering(config);
    if let Some(endpoint) = &config.endpoint {
        backend = backend.with_credential_source(
            &config.user_dir,
            &endpoint.base_url,
            endpoint.api_key_env.as_deref(),
        );
    }
    if let Some(id) = account_id {
        backend = backend.with_account(id);
    }
    if let Some(profile) = config.active_model_profile() {
        backend = backend.with_profile(profile.clone());
    } else if config.provider.as_deref() == Some(llm::chatgpt::PROVIDER)
        && let Some(profile) = llm::chatgpt::scoped_model_profile(
            &config.user_dir,
            backend.account_id(),
            &config.model,
        )?
    {
        backend = backend.with_profile(profile);
    }
    if let Some(namespace) = &config.provider_cache_namespace {
        backend = backend.with_cache_namespace(namespace, config)?;
    }
    Ok(backend)
}

pub fn make_session_backend(config: &Config, session: &Session) -> Result<Backend> {
    let mut scoped = config.clone();
    if config.provider.as_deref() == Some("chatgpt")
        && let Some(id) =
            session.control_state()["provider_identity"]["selection"]["account_id"].as_str()
    {
        ensure!(
            config
                .chatgpt_allowed_accounts
                .as_ref()
                .is_none_or(|ids| ids.contains(id)),
            "ChatGPT account is restricted by private global policy"
        );
        let mut selection = config.selection();
        selection.account_id = Some(id.to_owned());
        scoped = config.with_selection(&selection);
        scoped.chatgpt_pinned_accounts.insert(id.to_owned());
    }
    make_backend(&scoped).map(|backend| backend.with_session_id(session.id().to_owned()))
}

pub(in crate::agent) fn session_auth_config(
    config: &Config,
    backend: &Backend,
    session: &mut Session,
) -> Result<Config> {
    let mut scoped = config.clone();
    if let Some(id) = backend.account_id() {
        let mut selection = config.selection();
        selection.account_id = Some(id.to_owned());
        scoped = config.with_selection(&selection);
        scoped.chatgpt_pinned_accounts.insert(id.to_owned());
    }
    let mut state = session.control_state();
    if !state.is_object() {
        state = json!({});
    }
    scoped.provider_cache_namespace = Some(format!(
        "{}:history:{}",
        session.id(),
        state["provider_identity"]["history_generation"]
            .as_u64()
            .unwrap_or(0)
    ));
    let saved = state["provider_identity"]["role_accounts"].clone();
    let mut roles = serde_json::Map::new();
    for (role, selection) in &mut scoped.model_roles {
        if selection.provider != "chatgpt" {
            continue;
        }
        let old = saved.get(role).unwrap_or(&Value::Null);
        let remembered = (old["provider"] == selection.provider && old["model"] == selection.model)
            .then(|| old["account_id"].as_str())
            .flatten();
        let implicit =
            selection.account_id.is_none() && (remembered.is_none() || old["implicit"] == true);
        let id = llm::chatgpt::resolve_account(
            &config.user_dir,
            remembered
                .or(selection.account_id.as_deref())
                .or(backend.account_id()),
        )?;
        ensure!(
            config
                .chatgpt_allowed_accounts
                .as_ref()
                .is_none_or(|ids| ids.contains(&id)),
            "ChatGPT role account is restricted by private global policy"
        );
        ensure!(
            implicit
                || backend.account_id() == Some(id.as_str())
                || config.chatgpt_pinned_accounts.contains(&id),
            "ChatGPT role account is not authorized by private global policy"
        );
        selection.account_id = Some(id.clone());
        scoped.chatgpt_pinned_accounts.insert(id);
        let mut identity = serde_json::to_value(&*selection)?;
        identity["implicit"] = json!(implicit);
        roles.insert(role.clone(), identity);
    }
    state["provider_identity"]["role_accounts"] = Value::Object(roles);
    session.set_control_state(state)?;
    Ok(scoped)
}
pub(in crate::agent) fn memory_authority(
    session: &Session,
    task_depth: usize,
) -> crate::storage::memory_backend::MemoryAuthority {
    let history_generation = session.context_history_generation();
    crate::storage::memory_backend::MemoryAuthority {
        session_id: session.id().to_owned(),
        branch_id: format!("{}:history:{history_generation}", session.id()),
        revision: session.memory_revision().unwrap_or("root").to_owned(),
        task_depth,
    }
}

pub(in crate::agent) fn memory_authority_matches(
    session: &Session,
    authority: &crate::storage::memory_backend::MemoryAuthority,
    task_depth: usize,
) -> bool {
    if authority.session_id != session.id()
        || authority.revision != session.memory_revision().unwrap_or("root")
        || authority.task_depth != task_depth
    {
        return false;
    }
    let Some(generation) = authority
        .branch_id
        .strip_prefix(session.id())
        .and_then(|tail| tail.strip_prefix(":history:"))
    else {
        return false;
    };
    !generation.is_empty()
        && generation.bytes().all(|byte| byte.is_ascii_digit())
        && (generation.len() == 1 || !generation.starts_with('0'))
        && generation.parse::<u64>().ok() == Some(session.context_history_generation())
}
