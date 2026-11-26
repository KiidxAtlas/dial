//! Chat engine architecture baseline from llama.cpp b11669 and Atomic e110381a.
//! When updating either runtime, compare its src/llama-arch.cpp names and the model
//! factories in src/llama-model.cpp. Encoder, projector, audio and draft-only loaders
//! are not chat engines. Do not infer support from a filename or repository label.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum GgufEngines {
    #[default]
    Unknown,
    Both,
    AtomicOnly,
}

impl GgufEngines {
    pub fn for_architecture(arch: &str) -> Self {
        match arch {
            "afmoe" | "apertus" | "arcee" | "arctic" | "arwkv7" | "baichuan" | "bailingmoe"
            | "bailingmoe2" | "bailingmoe3" | "bitnet" | "bloom" | "chameleon" | "chatglm"
            | "clef" | "codeshell" | "cogvlm" | "cohere2" | "cohere2moe" | "command-r" | "dbrx"
            | "deci" | "deepseek" | "deepseek2" | "deepseek2-ocr" | "deepseek32" | "deepseek4"
            | "dots1" | "dots3note" | "dream" | "ernie4_5" | "ernie4_5-moe" | "exaone"
            | "exaone-moe" | "exaone4" | "falcon" | "falcon-h1" | "gemma" | "gemma2" | "gemma3"
            | "gemma3n" | "gemma4" | "glm-dsa" | "glm4" | "glm4moe" | "glm5-next" | "gpt-oss"
            | "gpt2" | "gptneox" | "granite" | "granite_swa" | "granitehybrid" | "granitemoe"
            | "graniteswitch" | "grok" | "grovemoe" | "hrm_text" | "hunyuan-dense"
            | "hunyuan-moe" | "hunyuan_vl" | "hy_v3" | "hy_v4" | "internlm2" | "jais" | "jais2"
            | "jamba" | "k2-horizon" | "kimi-k3" | "kimi-linear" | "laguna" | "lfm2"
            | "lfm2moe" | "llada" | "llada-moe" | "llama" | "llama4" | "maincoder"
            | "maion-coder" | "mamba" | "mamba2" | "maple" | "mellum" | "mimo2" | "minicpm"
            | "minicpm3" | "minimax-01" | "minimax-m2" | "minimax-m3" | "mistral3" | "mistral4"
            | "mpt" | "muse-glimmer" | "nanbeige" | "nemotron" | "nemotron_h"
            | "nemotron_h_moe" | "olmo" | "olmo2" | "olmoe" | "openelm" | "orion" | "paddleocr"
            | "phi2" | "phi3" | "phimoe" | "plamo" | "plamo2" | "plamo3" | "plm" | "qwen"
            | "qwen2" | "qwen2moe" | "qwen2vl" | "qwen3" | "qwen35" | "qwen35moe" | "qwen3moe"
            | "qwen3next" | "qwen3vl" | "qwen3vlmoe" | "qwen4exp" | "refact" | "rnd1" | "rwkv6"
            | "rwkv6qwen2" | "rwkv7" | "seed_oss" | "smallthinker" | "smollm3" | "spark2_5"
            | "stablelm" | "starcoder" | "starcoder2" | "step35" | "t5" | "talkie"
            | "vllm-decision2" | "xverse" => Self::Both,
            "beam" | "clover_moe" | "deepseek41" | "domynedge" | "flint" | "inkling2"
            | "inkling2_loop" | "kimi-k31" | "kolibri1" | "lfm3" | "limite" | "metis16"
            | "mistral-large4" | "muse-spark" | "ricursive" | "sensenova_u1" | "xing4_0"
            | "zaya1" => Self::AtomicOnly,
            _ => Self::Unknown,
        }
    }
}

/// The resolved loader for a local GGUF, before checking installation.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GgufLoader {
    LlamaCpp,
    Atomic,
}

impl GgufLoader {
    pub fn resolve(file: &std::path::Path, engine: super::LocalEngine) -> anyhow::Result<Self> {
        use super::LocalEngine;
        use crate::llm::local_format::{LocalFormat, gguf_info};
        let engines = gguf_info(file)
            .map(|info| info.engines())
            .unwrap_or_default();
        let choices = LocalEngine::choices(LocalFormat::Gguf, engines);
        if engine == LocalEngine::Auto {
            if engines == GgufEngines::AtomicOnly {
                // Atomic is the only loader for this architecture, so Auto uses it. Whether it
                // is installed is reported by `program`, never replaced by another runtime.
                anyhow::ensure!(
                    choices.contains(&LocalEngine::Atomic),
                    "No supported local engine for this GGUF on this platform. It requires Atomic Engine on Apple Silicon; choose another GGUF or a hosted endpoint."
                );
                return Ok(Self::Atomic);
            }
            // Keep the historical automatic path for unrecognized architectures.
            return Ok(Self::LlamaCpp);
        }
        anyhow::ensure!(
            choices.contains(&engine),
            "The selected {engine:?} engine is not a known loader for this model on this platform. Review the engine choice before loading it."
        );
        Ok(if engine == LocalEngine::Atomic {
            Self::Atomic
        } else {
            Self::LlamaCpp
        })
    }

    pub fn program(self) -> anyhow::Result<std::path::PathBuf> {
        use crate::llm::local_format::{atomic_server, llama_server};
        match self {
            Self::Atomic => atomic_server().ok_or_else(|| anyhow::anyhow!("Atomic Engine is not installed. Choose Atomic in Dial Desktop's model picker to set it up.")),
            Self::LlamaCpp => llama_server().ok_or_else(|| anyhow::anyhow!("GGUF models need llama.cpp's `llama-server`: run `/setup llama-cpp` to install it, or set DIAL_LLAMA_SERVER to an existing install")),
        }
    }
}
