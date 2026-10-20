//! Browser surfaces hosted outside Chromium: cmux browser splits.
//!
//! The backend is selected when a session has no Chromium driver yet: `browser.cmux` plus the
//! environment resolver decide. While it is selected the Chromium driver is not started. Actions
//! the backend cannot express are refused with an explicit error.

use serde_json::{Value, json};

use super::cmux::{CmuxClient, CmuxClientConfig, ResolveCmuxKindOptions, resolve_cmux_kind};
use crate::config::tools::browser::BrowserConfig;

const LOAD_TIMEOUT_MS: u64 = 30_000;
const LABEL: &str = "cmux";

/// A surface this session opened. `tab` is the id reported to the model; `handle` is the cmux
/// surface id behind it.
struct Tab {
    tab: Value,
    handle: String,
}

/// Per-tool external session. Tabs are addressed by the id the model was given; an action without
/// `tab` addresses the most recently opened tab. Tabs still open when the session ends are closed.
pub(super) struct Session {
    client: CmuxClient,
    tabs: Vec<Tab>,
}

/// Selects the cmux backend for `config`, if it is enabled and its environment is present.
pub(super) fn select(
    config: &BrowserConfig,
    env: impl Fn(&str) -> Option<String>,
) -> Option<Session> {
    let cmux = ResolveCmuxKindOptions {
        surface: None,
        setting_enabled: Some(config.cmux),
    };
    let kind = resolve_cmux_kind(&cmux, &env)?;
    Some(Session {
        client: CmuxClient::new(CmuxClientConfig::from_kind(&kind, &env)),
        tabs: Vec::new(),
    })
}

impl Session {
    #[cfg(test)]
    pub(super) fn label(&self) -> &'static str {
        LABEL
    }

    /// Runs one browser action on the external surface. Results use the Chromium shapes.
    pub(super) fn run(&mut self, action: &str, args: &Value) -> Result<Value, String> {
        cmux_action(&self.client, &mut self.tabs, action, args)
    }
}

impl Drop for Session {
    /// Closes the tabs still open when the session ends, so no surface outlives its tool.
    fn drop(&mut self) {
        for tab in self.tabs.drain(..) {
            let _ = self.client.close_surface(&tab.handle);
        }
    }
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{key} is required"))
}

fn artifact(data: &str) -> Value {
    json!({"artifact": {"media_type": "image/png", "encoding": "base64", "data": data}})
}

/// Whether the action names a tab with `tab`, rather than addressing the default tab.
fn names_tab(args: &Value) -> bool {
    !matches!(args.get("tab"), None | Some(Value::Null))
}

fn no_tab() -> String {
    format!("no open {LABEL} browser tab; run open first")
}

/// The tab id as a comparable key: strings and numbers both match their text form.
fn tab_key(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// Index of the tab `args.tab` names, or of the most recently opened tab when `args.tab` is absent.
fn resolve(tabs: &[Tab], args: &Value) -> Result<usize, String> {
    if !names_tab(args) {
        return tabs.len().checked_sub(1).ok_or_else(no_tab);
    }
    let key = tab_key(&args["tab"]).ok_or("tab must be a tab id string or number")?;
    tabs.iter()
        .position(|tab| tab_key(&tab.tab).as_deref() == Some(key.as_str()))
        .ok_or_else(|| format!("tab {key} is not open in this session"))
}

fn cmux_action(
    client: &CmuxClient,
    tabs: &mut Vec<Tab>,
    action: &str,
    args: &Value,
) -> Result<Value, String> {
    let err = |e: super::cmux::CmuxError| e.to_string();
    if action == "open" {
        let url = str_arg(args, "url")?;
        let opened = client.open_split(url, None, None).map_err(err)?;
        if url != "about:blank"
            && let Err(error) =
                client.wait_for_load(&opened.surface_id, "complete", LOAD_TIMEOUT_MS)
        {
            let _ = client.close_surface(&opened.surface_id);
            return Err(err(error));
        }
        tabs.push(Tab {
            tab: json!(opened.surface_id),
            handle: opened.surface_id.clone(),
        });
        return Ok(json!({"backend": LABEL, "tab": opened.surface_id, "url": url}));
    }
    if action == "tabs" && !names_tab(args) {
        if tabs.is_empty() {
            return Err(no_tab());
        }
        let mut listed = Vec::with_capacity(tabs.len());
        for tab in tabs.iter() {
            let url = client.url(&tab.handle).map_err(err)?;
            listed.push(json!({"tab": tab.tab, "url": url["url"]}));
        }
        return Ok(json!({"tabs": listed}));
    }
    let index = resolve(tabs, args)?;
    let id = tabs[index].handle.clone();
    match action {
        "navigate" => {
            let url = str_arg(args, "url")?;
            client.navigate(&id, url).map_err(err)?;
            client
                .wait_for_load(&id, "complete", LOAD_TIMEOUT_MS)
                .map_err(err)?;
            Ok(json!({"tab": id, "url": url}))
        }
        "evaluate" => {
            let expression = str_arg(args, "expression")?;
            let result = client.eval(&id, expression).map_err(err)?;
            Ok(json!({"tab": id, "value": result["value"]}))
        }
        "snapshot" => client.snapshot(&id, false, None).map_err(err),
        "screenshot" => {
            let result = client.screenshot(&id).map_err(err)?;
            let data = result["png_base64"]
                .as_str()
                .ok_or("cmux screenshot returned no png_base64")?;
            Ok(artifact(data))
        }
        "click" => {
            client.click(&id, str_arg(args, "selector")?).map_err(err)?;
            Ok(json!({"tab": id}))
        }
        "type" => {
            client
                .type_text(&id, str_arg(args, "selector")?, str_arg(args, "text")?)
                .map_err(err)?;
            Ok(json!({"tab": id}))
        }
        "tabs" => {
            let url = client.url(&id).map_err(err)?;
            Ok(json!({"tabs": [{"tab": id, "url": url["url"]}]}))
        }
        "close" => {
            client.close_surface(&id).map_err(err)?;
            tabs.remove(index);
            Ok(json!({"closed": true}))
        }
        other => Err(format!(
            "browser action {other:?} is not supported by the {LABEL} backend"
        )),
    }
}

#[cfg(test)]
mod tests;
