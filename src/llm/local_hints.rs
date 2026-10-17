//! Request-scoped hints the local-model profile (`[assistance.local]`) attaches to one request, and
//! the local server kind that decides how each hint reaches the wire. Hints never persist and never
//! reach hosted providers; a field the target server has no control for is not sent.

/// One request's local hints. `None`/`false` sends nothing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LocalHints {
    /// Template thinking switch: `chat_template_kwargs.enable_thinking` on llama.cpp and Dial's MLX
    /// runtime, `think` on Ollama. Set only when the server exposes a switch for this model and no
    /// explicit profile setting decides it.
    pub thinking: Option<bool>,
    /// Keep the model loaded this long after the request: Ollama `keep_alive` (seconds), LM Studio
    /// `ttl`. An explicit profile `keep_alive` wins.
    pub keep_alive_secs: Option<u32>,
    /// llama.cpp: `cache_prompt: true` and `id_slot` set to this slot.
    pub cache_slot: Option<u32>,
    /// llama.cpp: ask for streamed prompt-processing progress (`return_progress`).
    pub prompt_progress: bool,
    /// Loop recovery: presence penalty for this request only (never overrides a profile value).
    pub presence_penalty: Option<f64>,
    /// Loop recovery: repetition penalty for this request only (never overrides a profile value).
    pub repetition_penalty: Option<f64>,
}

impl LocalHints {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

impl LocalHints {
    /// `self` (the next request's hints) wins per field over `base` (the backend's persistent hints).
    /// Flags are set when either side sets them.
    pub fn over(self, base: &LocalHints) -> LocalHints {
        LocalHints {
            thinking: self.thinking.or(base.thinking),
            keep_alive_secs: self.keep_alive_secs.or(base.keep_alive_secs),
            cache_slot: self.cache_slot.or(base.cache_slot),
            prompt_progress: self.prompt_progress || base.prompt_progress,
            presence_penalty: self.presence_penalty.or(base.presence_penalty),
            repetition_penalty: self.repetition_penalty.or(base.repetition_penalty),
        }
    }

    /// Drops the hints an explicit profile setting already decides: a profile `thinking` or
    /// `reasoning_effort` owns the thinking switch, and a profile `keep_alive` owns the duration.
    pub fn without_explicit(mut self, thinking_decided: bool, keep_alive_decided: bool) -> Self {
        if thinking_decided {
            self.thinking = None;
        }
        if keep_alive_decided {
            self.keep_alive_secs = None;
        }
        self
    }

    /// Writes the hints one OpenAI-compatible request body understands. Every field is written only
    /// when the body does not already carry it, so explicit profile values applied earlier win.
    /// Hosted routes never reach this (`LocalServer::Other` for them) and servers without a verified
    /// control for a hint send nothing for it.
    pub fn apply_openai(&self, server: LocalServer, body: &mut serde_json::Value) {
        let Some(object) = body.as_object_mut() else {
            return;
        };
        match server {
            // llama-server: /completion's cache_prompt, id_slot, return_progress, repeat_penalty and
            // presence_penalty; chat_template_kwargs is documented on /v1/chat/completions.
            LocalServer::LlamaCpp => {
                if let Some(slot) = self.cache_slot {
                    put_absent(object, "cache_prompt", true.into());
                    put_absent(object, "id_slot", slot.into());
                }
                if self.prompt_progress {
                    put_absent(object, "return_progress", true.into());
                }
                put_thinking(object, self.thinking);
                put_penalties(object, self, "repeat_penalty");
            }
            // Dial's mlx_lm.server takes the same names LocalSampling::apply sends.
            LocalServer::MlxRuntime => {
                put_thinking(object, self.thinking);
                put_penalties(object, self, "repetition_penalty");
            }
            // LM Studio's OpenAI-compatible API documents `ttl` (idle seconds before unload, applies to
            // JIT-loaded models), `presence_penalty` and `repeat_penalty`. It documents no request-level
            // thinking switch, so none is sent.
            LocalServer::LmStudio => {
                if let Some(secs) = self.keep_alive_secs {
                    put_absent(object, "ttl", secs.into());
                }
                put_penalties(object, self, "repeat_penalty");
            }
            // Ollama's native API is applied by `apply_ollama`; its OpenAI-compatible route is not used.
            LocalServer::Ollama | LocalServer::Other => {}
        }
    }

    /// Writes the hints an Ollama `/api/chat` body understands: `keep_alive` and `think` on the body,
    /// penalties in `options` (both documented in Ollama's API reference). Fields already present stay.
    pub fn apply_ollama(&self, body: &mut serde_json::Value, options: &mut serde_json::Value) {
        if let (Some(object), Some(secs)) = (body.as_object_mut(), self.keep_alive_secs) {
            put_absent(object, "keep_alive", format!("{secs}s").into());
        }
        if let Some(object) = body.as_object_mut() {
            put_thinking_flag(object, self.thinking);
        }
        if let Some(object) = options.as_object_mut() {
            put_penalties(object, self, "repeat_penalty");
        }
    }
}

fn put_absent(
    object: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    value: serde_json::Value,
) {
    object.entry(key).or_insert(value);
}

/// `chat_template_kwargs.enable_thinking`, merged into any existing kwargs object.
fn put_thinking(object: &mut serde_json::Map<String, serde_json::Value>, thinking: Option<bool>) {
    let Some(thinking) = thinking else {
        return;
    };
    let kwargs = object
        .entry("chat_template_kwargs")
        .or_insert_with(|| serde_json::json!({}));
    if let Some(kwargs) = kwargs.as_object_mut() {
        put_absent(kwargs, "enable_thinking", thinking.into());
    }
}

fn put_thinking_flag(
    object: &mut serde_json::Map<String, serde_json::Value>,
    thinking: Option<bool>,
) {
    if let Some(thinking) = thinking {
        put_absent(object, "think", thinking.into());
    }
}

/// Penalties under the server's own name for repetition control; finite values only.
fn put_penalties(
    object: &mut serde_json::Map<String, serde_json::Value>,
    hints: &LocalHints,
    repetition_name: &str,
) {
    if let Some(value) = hints.presence_penalty.filter(|v| v.is_finite()) {
        put_absent(object, "presence_penalty", value.into());
    }
    if let Some(value) = hints.repetition_penalty.filter(|v| v.is_finite()) {
        put_absent(object, repetition_name, value.into());
    }
}

impl LocalServer {
    /// The server behind a request. `base_url` is `None` for Dial-launched runtimes. A non-local
    /// base URL is always `Other`: hints never reach a hosted route, whatever the provider id.
    pub fn detect(
        provider: &str,
        model: &str,
        base_url: Option<&str>,
        token: Option<&str>,
        headers: &[crate::llm::model_config::RequestHeader],
    ) -> Self {
        if let Some(base) = base_url {
            let local =
                crate::llm::host_of(base).is_some_and(|host| crate::llm::is_local_host(&host));
            if !local {
                return Self::Other;
            }
        }
        match (provider, base_url) {
            ("mlx", None) if crate::llm::local_format::serves_with_llama_cpp(model) => {
                Self::LlamaCpp
            }
            ("mlx", None) => Self::MlxRuntime,
            ("lm-studio", _) => Self::LmStudio,
            ("ollama", _) => Self::Ollama,
            ("llama-cpp", _) => Self::LlamaCpp,
            (_, Some(base)) if crate::llm::catalog::is_llama_server(base, token, headers) => {
                Self::LlamaCpp
            }
            _ => Self::Other,
        }
    }
}

/// The local server behind a request, which decides each hint's wire name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalServer {
    /// Dial's own `mlx_lm.server` runtime.
    MlxRuntime,
    /// llama.cpp `llama-server`: Dial-launched for GGUF models, the `llama-cpp` provider, or an
    /// endpoint recognised by `catalog::is_llama_server`.
    LlamaCpp,
    /// LM Studio's OpenAI-compatible API (`lm-studio` provider).
    LmStudio,
    /// Ollama's native `/api/chat` (`ollama` provider).
    Ollama,
    /// Another local OpenAI-compatible server: no hint is sent.
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn all() -> LocalHints {
        LocalHints {
            thinking: Some(false),
            keep_alive_secs: Some(1800),
            cache_slot: Some(3),
            prompt_progress: true,
            presence_penalty: Some(0.5),
            repetition_penalty: Some(1.2),
        }
    }

    #[test]
    fn llama_cpp_wire_uses_completion_fields_and_merges_template_kwargs() {
        let mut body = json!({"model":"m","chat_template_kwargs":{"other":1}});
        all().apply_openai(LocalServer::LlamaCpp, &mut body);
        assert_eq!(body["cache_prompt"], true);
        assert_eq!(body["id_slot"], 3);
        assert_eq!(body["return_progress"], true);
        assert_eq!(
            body["chat_template_kwargs"],
            json!({"other":1,"enable_thinking":false})
        );
        assert_eq!(body["presence_penalty"], 0.5);
        assert_eq!(body["repeat_penalty"], 1.2);
        assert!(body.get("ttl").is_none() && body.get("think").is_none());
    }

    #[test]
    fn mlx_runtime_wire_uses_template_kwargs_and_mlx_penalty_names_only() {
        let mut body = json!({"model":"m"});
        all().apply_openai(LocalServer::MlxRuntime, &mut body);
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(body["repetition_penalty"], 1.2);
        assert_eq!(body["presence_penalty"], 0.5);
        assert!(body.get("repeat_penalty").is_none());
        assert!(body.get("cache_prompt").is_none() && body.get("ttl").is_none());
    }

    #[test]
    fn lm_studio_wire_uses_documented_ttl_and_penalties_without_thinking() {
        let mut body = json!({"model":"m"});
        all().apply_openai(LocalServer::LmStudio, &mut body);
        assert_eq!(body["ttl"], 1800);
        assert_eq!(body["repeat_penalty"], 1.2);
        assert_eq!(body["presence_penalty"], 0.5);
        assert!(body.get("chat_template_kwargs").is_none());
        assert!(body.get("think").is_none() && body.get("cache_prompt").is_none());
    }

    #[test]
    fn ollama_wire_puts_keep_alive_and_think_on_body_and_penalties_in_options() {
        let mut body = json!({"model":"m","options":{}});
        let mut options = body["options"].take();
        all().apply_ollama(&mut body, &mut options);
        body["options"] = options;
        assert_eq!(body["keep_alive"], "1800s");
        assert_eq!(body["think"], false);
        assert_eq!(body["options"]["repeat_penalty"], 1.2);
        assert_eq!(body["options"]["presence_penalty"], 0.5);
        assert!(body.get("ttl").is_none() && body.get("chat_template_kwargs").is_none());
    }

    #[test]
    fn existing_body_fields_win_over_hints_on_every_server() {
        let mut body = json!({"ttl":60,"repeat_penalty":1.0,"presence_penalty":0.1,"cache_prompt":false,
            "chat_template_kwargs":{"enable_thinking":true}});
        all().apply_openai(LocalServer::LlamaCpp, &mut body);
        assert_eq!(body["cache_prompt"], false);
        assert_eq!(body["repeat_penalty"], 1.0);
        assert_eq!(body["presence_penalty"], 0.1);
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], true);

        let mut body = json!({"keep_alive":"5m","think":true,"options":{"repeat_penalty":1.05}});
        let mut options = body["options"].take();
        all().apply_ollama(&mut body, &mut options);
        body["options"] = options;
        assert_eq!(body["keep_alive"], "5m");
        assert_eq!(body["think"], true);
        assert_eq!(body["options"]["repeat_penalty"], 1.05);
        assert_eq!(body["options"]["presence_penalty"], 0.5);
    }

    #[test]
    fn ollama_and_other_servers_send_nothing_through_the_openai_path() {
        for server in [LocalServer::Ollama, LocalServer::Other] {
            let mut body = json!({"model":"m"});
            all().apply_openai(server, &mut body);
            assert_eq!(body, json!({"model":"m"}));
        }
    }

    #[test]
    fn next_hints_override_base_per_field_and_explicit_profile_fields_drop_hints() {
        let base = LocalHints {
            keep_alive_secs: Some(60),
            cache_slot: Some(1),
            ..Default::default()
        };
        let next = LocalHints {
            cache_slot: Some(2),
            thinking: Some(true),
            prompt_progress: true,
            ..Default::default()
        };
        let merged = next.over(&base);
        assert_eq!(merged.keep_alive_secs, Some(60));
        assert_eq!(merged.cache_slot, Some(2));
        assert_eq!(merged.thinking, Some(true));
        assert!(merged.prompt_progress);
        let kept = merged.clone().without_explicit(true, true);
        assert_eq!(kept.thinking, None);
        assert_eq!(kept.keep_alive_secs, None);
        assert_eq!(kept.cache_slot, Some(2));
        assert!(LocalHints::default().is_empty() && !merged.is_empty());
    }

    #[test]
    fn detect_never_treats_non_local_routes_as_local_servers() {
        for provider in ["lm-studio", "ollama", "llama-cpp", "mlx", "openai"] {
            assert_eq!(
                LocalServer::detect(provider, "m", Some("https://api.example.com/v1"), None, &[]),
                LocalServer::Other,
                "{provider}"
            );
        }
        assert_eq!(
            LocalServer::detect(
                "lm-studio",
                "m",
                Some("http://127.0.0.1:1234/v1"),
                None,
                &[]
            ),
            LocalServer::LmStudio
        );
        assert_eq!(
            LocalServer::detect("ollama", "m", Some("http://localhost:11434"), None, &[]),
            LocalServer::Ollama
        );
        assert_eq!(
            LocalServer::detect(
                "llama-cpp",
                "m",
                Some("http://127.0.0.1:8080/v1"),
                None,
                &[]
            ),
            LocalServer::LlamaCpp
        );
        assert_eq!(
            LocalServer::detect("mlx", "/nonexistent/model-dir", None, None, &[]),
            LocalServer::MlxRuntime
        );
    }
}
