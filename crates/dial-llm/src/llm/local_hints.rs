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
mod tests;
