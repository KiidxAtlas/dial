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
mod tests {
    use super::*;
    #[test]
    fn lmstudio_profiles_round_trip_and_use_supported_wire_names() {
        use crate::llm::profiles::{ModelCapabilities, ModelProfile};
        let mut profile = ModelProfile {
            provider: "lm-studio".into(),
            model: "qwen".into(),
            capabilities: ModelCapabilities {
                seed: true,
                ..Default::default()
            },
            local_sampling: Some(LocalSampling {
                top_k: Some(40),
                min_p: Some(0.05),
                seed: Some(7),
                repetition_penalty: Some(1.1),
                presence_penalty: Some(0.2),
                frequency_penalty: Some(0.3),
                ..Default::default()
            }),
            ..Default::default()
        };
        profile.validate().unwrap();
        let encoded = toml::to_string(&profile).unwrap();
        profile = toml::from_str(&encoded).unwrap();
        let mut body = serde_json::json!({"max_tokens":32});
        profile.apply_openai(&mut body);
        assert_eq!(body["top_k"], 40);
        assert_eq!(body["min_p"], 0.05);
        assert_eq!(body["seed"], 7);
        assert_eq!(body["repeat_penalty"], 1.1);
        assert_eq!(body["presence_penalty"], 0.2);
        assert_eq!(body["frequency_penalty"], 0.3);
        assert!(body.get("repetition_penalty").is_none());
        profile.local_sampling.as_mut().unwrap().xtc_probability = Some(0.1);
        assert!(
            profile
                .validate()
                .unwrap_err()
                .to_string()
                .contains("not supported by LM Studio")
        );
        let mut body = serde_json::json!({"max_tokens":32});
        profile.apply_openai(&mut body);
        assert!(body.get("xtc_probability").is_none());
        profile.provider = "openai".into();
        assert!(profile.validate().is_err());
        let mut body = serde_json::json!({"max_tokens":32});
        profile.apply_openai(&mut body);
        assert!(body.get("top_k").is_none());
        assert!(body.get("repeat_penalty").is_none());
    }

    #[test]
    fn local_sampling_validates_and_omits_unspecified_options() {
        let sampling = LocalSampling {
            top_k: Some(40),
            min_p: Some(0.05),
            seed: Some(7),
            ..Default::default()
        };
        sampling.validate().unwrap();
        let mut body = serde_json::json!({"temperature":0.2});
        sampling.apply(&mut body);
        assert_eq!(body["top_k"], 40);
        assert_eq!(body["seed"], 7);
        assert!(body.get("presence_penalty").is_none());
        assert!(
            LocalSampling {
                min_p: Some(1.1),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }
    #[test]
    fn profile_controls_round_trip_and_never_leak_to_cloud_requests() {
        use crate::llm::profiles::{ModelCapabilities, ModelProfile};
        let mut profile = ModelProfile {
            provider: "mlx".into(),
            model: "test-model".into(),
            capabilities: ModelCapabilities {
                seed: true,
                ..Default::default()
            },
            local_sampling: Some(LocalSampling {
                top_k: Some(40),
                seed: Some(7),
                ..Default::default()
            }),
            ..Default::default()
        };
        profile.validate().unwrap();
        let encoded = toml::to_string(&profile).unwrap();
        let restored: ModelProfile = toml::from_str(&encoded).unwrap();
        assert_eq!(restored.local_sampling.unwrap().top_k, Some(40));
        let mut body = serde_json::json!({"max_tokens":100});
        profile.apply_openai(&mut body);
        assert_eq!(body["top_k"], 40);
        assert_eq!(body["seed"], 7);
        profile.provider = "openai".into();
        assert!(profile.validate().is_err());
        let mut body = serde_json::json!({"max_tokens":100});
        profile.apply_openai(&mut body);
        assert!(body.get("top_k").is_none());
        assert!(body.get("seed").is_none());
    }
}
