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

/// Map the `--approval-mode` values onto Dial's `--approval` values (`ask|write|auto|readonly`).
pub fn approval_alias(value: &str) -> Result<&'static str, String> {
    match value {
        "always-ask" => Ok("ask"),
        "write" => Ok("write"),
        "yolo" => Ok("auto"),
        other => Err(format!(
            "invalid --approval-mode {other:?}; expected one of: always-ask, write, yolo"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_mode_keeps_the_default_chat_path() {
        assert_eq!(resolve(None, false).unwrap(), None);
    }

    #[test]
    fn json_mode_is_the_event_stream_on_the_chat_path() {
        let r = resolve(Some("json"), false).unwrap().unwrap();
        assert_eq!(r.route, Route::Chat);
        assert_eq!(r.output, OutputFormat::StreamJson);
    }

    #[test]
    fn protocol_modes_route_to_their_servers() {
        assert_eq!(
            resolve(Some("rpc"), false).unwrap().unwrap().route,
            Route::Rpc
        );
        assert_eq!(
            resolve(Some("rpc-ui"), false).unwrap().unwrap().route,
            Route::Rpc
        );
        assert_eq!(
            resolve(Some("acp"), false).unwrap().unwrap().route,
            Route::Acp
        );
    }

    #[test]
    fn unknown_mode_is_rejected_with_the_valid_set() {
        let error = resolve(Some("gui"), false).unwrap_err();
        assert!(error.contains("text, json, rpc, acp, rpc-ui"), "{error}");
    }

    #[test]
    fn no_ui_requires_an_rpc_mode() {
        assert!(resolve(None, true).is_err());
        assert!(resolve(Some("json"), true).is_err());
        assert!(resolve(Some("rpc"), true).unwrap().is_some());
        assert!(resolve(Some("rpc-ui"), true).unwrap().is_some());
    }

    #[test]
    fn approval_aliases_map_onto_dial_policies() {
        assert_eq!(approval_alias("always-ask").unwrap(), "ask");
        assert_eq!(approval_alias("yolo").unwrap(), "auto");
        assert_eq!(approval_alias("write").unwrap(), "write");
        assert!(approval_alias("nope").is_err());
    }
}
