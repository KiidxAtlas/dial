//! Request-level sampling knobs for MLX, llama.cpp, LM Studio and Ollama, not cloud APIs.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalSampling {
    pub top_k: Option<u32>,
    pub min_p: Option<f64>,
    pub repetition_penalty: Option<f64>,
    pub repetition_context_size: Option<u32>,
    pub presence_penalty: Option<f64>,
    pub presence_context_size: Option<u32>,
    pub frequency_penalty: Option<f64>,
    pub frequency_context_size: Option<u32>,
    pub xtc_probability: Option<f64>,
    pub xtc_threshold: Option<f64>,
    pub seed: Option<u32>,
    pub chat_template_kwargs: Option<serde_json::Map<String, serde_json::Value>>,
}
impl LocalSampling {
    /// LM Studio's OpenAI-compatible API accepts only this subset of local controls.
    pub fn lmstudio_field(name: &str) -> bool {
        matches!(
            name,
            "top_k"
                | "min_p"
                | "seed"
                | "repetition_penalty"
                | "presence_penalty"
                | "frequency_penalty"
        )
    }

    pub fn validate_lmstudio(&self) -> Result<()> {
        self.validate()?;
        let values = serde_json::to_value(self)?;
        for (name, value) in values.as_object().expect("sampling object") {
            ensure!(
                value.is_null() || Self::lmstudio_field(name),
                "{name} is not supported by LM Studio"
            );
        }
        Ok(())
    }

    pub fn apply_lmstudio(&self, body: &mut serde_json::Value) {
        if let Ok(serde_json::Value::Object(values)) = serde_json::to_value(self) {
            for (name, value) in values {
                if !value.is_null() && Self::lmstudio_field(&name) {
                    let wire_name = if name == "repetition_penalty" {
                        "repeat_penalty"
                    } else {
                        &name
                    };
                    body[wire_name] = value;
                }
            }
        }
    }

    /// Ollama's `/api/chat` reads sampling from `options` under llama.cpp-style names; the separate
    /// presence/frequency windows, XTC and chat-template arguments have no Ollama equivalent.
    pub fn ollama_field(name: &str) -> bool {
        Self::ollama_name(name).is_some()
    }

    fn ollama_name(name: &str) -> Option<&'static str> {
        Some(match name {
            "top_k" => "top_k",
            "min_p" => "min_p",
            "seed" => "seed",
            "repetition_penalty" => "repeat_penalty",
            "repetition_context_size" => "repeat_last_n",
            "presence_penalty" => "presence_penalty",
            "frequency_penalty" => "frequency_penalty",
            _ => return None,
        })
    }

    pub fn validate_ollama(&self) -> Result<()> {
        self.validate()?;
        let values = serde_json::to_value(self)?;
        for (name, value) in values.as_object().expect("sampling object") {
            ensure!(
                value.is_null() || Self::ollama_name(name).is_some(),
                "{name} is not supported by Ollama"
            );
        }
        Ok(())
    }

    /// Writes the supported controls into an Ollama request's `options` object.
    pub fn apply_ollama(&self, options: &mut serde_json::Value) {
        if let Ok(serde_json::Value::Object(values)) = serde_json::to_value(self) {
            for (name, value) in values {
                if let Some(wire_name) = Self::ollama_name(&name).filter(|_| !value.is_null()) {
                    options[wire_name] = value;
                }
            }
        }
    }

    pub fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("min_p", self.min_p),
            ("xtc_probability", self.xtc_probability),
            ("xtc_threshold", self.xtc_threshold),
        ] {
            ensure!(
                value.is_none_or(|v| v.is_finite() && (0.0..=1.0).contains(&v)),
                "{name} must be 0..1"
            );
        }
        ensure!(
            self.repetition_penalty
                .is_none_or(|v| v.is_finite() && v >= 0.0),
            "repetition_penalty must be finite and non-negative"
        );
        ensure!(
            self.presence_penalty.is_none_or(f64::is_finite)
                && self.frequency_penalty.is_none_or(f64::is_finite),
            "penalties must be finite"
        );
        Ok(())
    }
    pub fn apply(&self, body: &mut serde_json::Value) {
        if let Ok(serde_json::Value::Object(values)) = serde_json::to_value(self) {
            for (name, value) in values {
                if !value.is_null() {
                    body[&name] = value;
                }
            }
        }
    }

    /// llama.cpp's `llama-server` (Dial-launched GGUF models) names repetition controls
    /// differently and applies one penalty window to every penalty, so the separate presence and
    /// frequency windows have no equivalent and are not sent.
    pub fn apply_llamacpp(&self, body: &mut serde_json::Value) {
        if let Ok(serde_json::Value::Object(values)) = serde_json::to_value(self) {
            for (name, value) in values {
                let wire_name = match name.as_str() {
                    "repetition_penalty" => "repeat_penalty",
                    "repetition_context_size" => "repeat_last_n",
                    "presence_context_size" | "frequency_context_size" => continue,
                    other => other,
                };
                if !value.is_null() {
                    body[wire_name] = value;
                }
            }
        }
    }
}
#[cfg(test)]
mod tests;
