//! `--mode`, `--no-ui` and `--approval-mode` resolution.
//!
//! `--mode` selects the protocol the process speaks. Dial's existing
//! surfaces map onto it as follows: `text` is the print/interactive text path,
//! `json` is the event stream (`--output-format stream-json`), `rpc`/`rpc-ui`
//! is the JSON-lines control protocol and `acp` is the ACP stdio adapter.

use crate::repl::OutputFormat;

/// Which runtime surface a resolved `--mode` hands control to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// The chat/print path in `main.rs`.
    Chat,
    /// The JSON-lines RPC control protocol.
    Rpc,
    /// The ACP v1 stdio adapter.
    Acp,
}

/// The resolved effect of `--mode` (or its absence).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub route: Route,
    /// Stdout format for chat/print turns.
    pub output: OutputFormat,
}

/// Parse `--mode`; `None` keeps Dial's default chat path with the given `--output-format`.
pub fn resolve(mode: Option<&str>, no_ui: bool) -> Result<Option<Resolved>, String> {
    let resolved = match mode {
        None => None,
        Some("text") => Some(Resolved {
            route: Route::Chat,
            output: OutputFormat::Text,
        }),
        Some("json") => Some(Resolved {
            route: Route::Chat,
            output: OutputFormat::StreamJson,
        }),
        Some("rpc" | "rpc-ui") => Some(Resolved {
            route: Route::Rpc,
            output: OutputFormat::Text,
        }),
        Some("acp") => Some(Resolved {
            route: Route::Acp,
            output: OutputFormat::Text,
        }),
        Some(other) => {
            return Err(format!(
                "invalid --mode {other:?}; expected one of: text, json, rpc, acp, rpc-ui"
            ));
        }
    };
    let is_rpc = matches!(
        resolved,
        Some(Resolved {
            route: Route::Rpc,
            ..
        })
    );
    if no_ui && !is_rpc {
        return Err("--no-ui requires --mode rpc or --mode rpc-ui".into());
    }
    Ok(resolved)
}

/// Compatibility spellings of approval policies, shared by parsing and CLI choices.
pub const APPROVAL_ALIASES: &[(&str, &str)] =
    &[("always-ask", "ask"), ("write", "write"), ("yolo", "auto")];

/// Map the `--approval-mode` values onto canonical approval policy names.
pub fn approval_alias(value: &str) -> Result<&'static str, String> {
    APPROVAL_ALIASES
        .iter()
        .find_map(|&(alias, policy)| (alias == value).then_some(policy))
        .ok_or_else(|| {
            let names = APPROVAL_ALIASES
                .iter()
                .map(|&(alias, _)| alias)
                .collect::<Vec<_>>();
            format!(
                "invalid --approval-mode {value:?}; expected one of: {}",
                names.join(", ")
            )
        })
}

#[cfg(test)]
mod tests;
