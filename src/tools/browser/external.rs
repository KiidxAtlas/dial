//! Browser surfaces hosted outside Chromium: cmux browser splits and Tern browser panes.
//!
//! A backend is selected when a session has no Chromium driver yet: `browser.cmux` / `browser.tern`
//! plus the environment resolvers decide. While a backend is selected the Chromium driver is not
//! started. Actions the backend cannot express are refused with an explicit error.

use serde_json::{Value, json};

use super::BrowserConfig;
use super::cmux::{CmuxClient, CmuxClientConfig, ResolveCmuxKindOptions, resolve_cmux_kind};
use super::tern::{ResolveTernKindOptions, TernClient, resolve_tern_kind};

const LOAD_TIMEOUT_MS: u64 = 30_000;
const TERN_VIEWPORT: (u32, u32) = (1280, 720);

enum Backend {
    Cmux {
        client: CmuxClient,
        tabs: Vec<Tab<String>>,
    },
    Tern {
        client: TernClient,
        owner: u64,
        tabs: Vec<Tab<u64>>,
    },
}

/// A tab this session opened. `tab` is the id reported to the model; `handle` is the cmux surface
/// id or the Tern block behind it.
struct Tab<H> {
    tab: Value,
    handle: H,
}

/// Per-tool external session. Tabs are addressed by the id the model was given; an action without
/// `tab` addresses the most recently opened tab. Tabs still open when the session ends are closed.
pub(super) struct Session {
    backend: Backend,
}

/// Failure of an external action. `Unavailable` means the Tern wire cannot host the request
/// (no window, unsupported platform, refused or failed connection): the only case that falls
/// back to Chromium in auto mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ExternalError {
    Unavailable(String),
    Failed(String),
}

impl From<String> for ExternalError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

impl From<&str> for ExternalError {
    fn from(message: &str) -> Self {
        Self::Failed(message.to_owned())
    }
}

impl ExternalError {
    /// The tool-facing error text, without the variant.
    pub(super) fn into_message(self) -> String {
        match self {
            Self::Unavailable(message) | Self::Failed(message) => message,
        }
    }
}

/// Selects the external backend for `config`, if one is enabled and its environment is present.
pub(super) fn select(
    config: &BrowserConfig,
    env: impl Fn(&str) -> Option<String>,
) -> Option<Session> {
    let cmux = ResolveCmuxKindOptions {
        surface: None,
        setting_enabled: Some(config.cmux),
    };
    if let Some(kind) = resolve_cmux_kind(&cmux, &env) {
        let client = CmuxClient::new(CmuxClientConfig::from_kind(&kind, &env));
        return Some(Session::new(Backend::Cmux {
            client,
            tabs: Vec::new(),
        }));
    }
    let tern = ResolveTernKindOptions {
        setting_enabled: Some(config.tern),
    };
    let kind = resolve_tern_kind(&tern, &env)?;
    let owner = kind.pane;
    Some(Session::new(Backend::Tern {
        client: TernClient::new(&kind),
        owner,
        tabs: Vec::new(),
    }))
}

impl Session {
    fn new(backend: Backend) -> Self {
        Self { backend }
    }

    pub(super) fn label(&self) -> &'static str {
        match self.backend {
            Backend::Cmux { .. } => "cmux",
            Backend::Tern { .. } => "tern",
        }
    }

    /// Runs one browser action on the external surface. Results use the Chromium shapes.
    pub(super) fn run(&mut self, action: &str, args: &Value) -> Result<Value, ExternalError> {
        let label = self.label();
        match &mut self.backend {
            Backend::Cmux { client, tabs } => {
                cmux_action(client, tabs, label, action, args).map_err(ExternalError::Failed)
            }
            Backend::Tern {
                client,
                owner,
                tabs,
            } => tern_action(client, *owner, tabs, label, action, args),
        }
    }
}

impl Drop for Session {
    /// Closes the tabs still open when the session ends, so no surface or pane outlives its tool.
    fn drop(&mut self) {
        match &mut self.backend {
            Backend::Cmux { client, tabs } => {
                for tab in tabs.drain(..) {
                    let _ = client.close_surface(&tab.handle);
                }
            }
            Backend::Tern { client, tabs, .. } => {
                for tab in tabs.drain(..) {
                    let _ = client.close_tab(tab.handle);
                }
            }
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

fn no_tab(label: &str) -> String {
    format!("no open {label} browser tab; run open first")
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
fn resolve<H>(tabs: &[Tab<H>], args: &Value, label: &str) -> Result<usize, String> {
    if !names_tab(args) {
        return tabs.len().checked_sub(1).ok_or_else(|| no_tab(label));
    }
    let key = tab_key(&args["tab"]).ok_or("tab must be a tab id string or number")?;
    tabs.iter()
        .position(|tab| tab_key(&tab.tab).as_deref() == Some(key.as_str()))
        .ok_or_else(|| format!("tab {key} is not open in this session"))
}

fn cmux_action(
    client: &CmuxClient,
    tabs: &mut Vec<Tab<String>>,
    label: &'static str,
    action: &str,
    args: &Value,
) -> Result<Value, String> {
    let err = |e: super::cmux::CmuxError| e.to_string();
    if action == "open" {
        let url = str_arg(args, "url")?;
        let opened = client.open_split(url, None, None).map_err(err)?;
        if url != "about:blank" {
            if let Err(error) =
                client.wait_for_load(&opened.surface_id, "complete", LOAD_TIMEOUT_MS)
            {
                let _ = client.close_surface(&opened.surface_id);
                return Err(err(error));
            }
        }
        tabs.push(Tab {
            tab: json!(opened.surface_id),
            handle: opened.surface_id.clone(),
        });
        return Ok(json!({"backend": label, "tab": opened.surface_id, "url": url}));
    }
    if action == "tabs" && !names_tab(args) {
        if tabs.is_empty() {
            return Err(no_tab(label));
        }
        let mut listed = Vec::with_capacity(tabs.len());
        for tab in tabs.iter() {
            let url = client.url(&tab.handle).map_err(err)?;
            listed.push(json!({"tab": tab.tab, "url": url["url"]}));
        }
        return Ok(json!({"tabs": listed}));
    }
    let index = resolve(tabs, args, label)?;
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
            "browser action {other:?} is not supported by the {label} backend"
        )),
    }
}

fn tern_action(
    client: &TernClient,
    owner: u64,
    tabs: &mut Vec<Tab<u64>>,
    label: &'static str,
    action: &str,
    args: &Value,
) -> Result<Value, ExternalError> {
    let err = tern_error;
    if action == "open" {
        let url = str_arg(args, "url")?;
        let id = client
            .open_tab(owner, TERN_VIEWPORT.0, TERN_VIEWPORT.1)
            .map_err(err)?;
        if url != "about:blank" {
            if let Err(error) = client.goto(id, url) {
                let _ = client.close_tab(id);
                return Err(err(error));
            }
        }
        tabs.push(Tab {
            tab: json!(id),
            handle: id,
        });
        return Ok(json!({"backend": label, "tab": id, "url": url}));
    }
    if action == "fork" {
        let dir = match args.get("dir").and_then(Value::as_str) {
            None => None,
            Some("right") => Some(super::tern::ForkDir::Right),
            Some("down") => Some(super::tern::ForkDir::Down),
            Some(other) => return Err(format!("dir must be right or down, not {other:?}").into()),
        };
        require_op(client, "fork")?;
        let block = client.fork(owner, dir).map_err(err)?;
        return Ok(json!({"backend": label, "block": block}));
    }
    if action == "tabs" && !names_tab(args) {
        if tabs.is_empty() {
            return Err(no_tab(label).into());
        }
        let mut listed = Vec::with_capacity(tabs.len());
        for tab in tabs.iter() {
            listed.push(client.state(tab.handle).map_err(err)?);
        }
        return Ok(json!({"tabs": listed}));
    }
    let index = resolve(tabs, args, label)?;
    let id = tabs[index].handle;
    match action {
        "navigate" => {
            let url = str_arg(args, "url")?;
            client.goto(id, url).map_err(err)?;
            Ok(json!({"tab": id, "url": url}))
        }
        "evaluate" => {
            let expression = str_arg(args, "expression")?;
            let function = format!("() => ({expression})");
            let value = client.eval(id, &function, json!([]), "page").map_err(err)?;
            Ok(json!({"tab": id, "value": value}))
        }
        "screenshot" => {
            let data = client.capture(id, "png", None).map_err(err)?;
            Ok(artifact(&data))
        }
        "tabs" => Ok(json!({"tabs": [client.state(id).map_err(err)?]})),
        "close" => {
            client.close_tab(id).map_err(err)?;
            tabs.remove(index);
            Ok(json!({"closed": true}))
        }
        "back" | "forward" | "reload" => {
            require_op(client, "nav")?;
            client.nav(id, action).map_err(err)?;
            Ok(json!({"tab": id, "action": action}))
        }
        "events" => {
            require_op(client, "events")?;
            let after = match args.get("after").filter(|value| !value.is_null()) {
                None => 0,
                Some(value) => value
                    .as_u64()
                    .ok_or("after must be a non-negative integer")?,
            };
            let events = client.events(id, after).map_err(err)?;
            Ok(json!({"tab": id, "events": events}))
        }
        other => {
            Err(format!("browser action {other:?} is not supported by the {label} backend").into())
        }
    }
}

fn tern_error(error: super::tern::TernError) -> ExternalError {
    let unavailable = error.is_unavailable();
    let message = format!("{} [{}]", error.message, error.kind.as_str());
    if unavailable {
        ExternalError::Unavailable(message)
    } else {
        ExternalError::Failed(message)
    }
}

/// Refuses an op the connected Tern daemon's welcome does not advertise.
fn require_op(client: &TernClient, op: &str) -> Result<(), ExternalError> {
    if client.supports(op).map_err(tern_error)? {
        Ok(())
    } else {
        Err(format!("Tern daemon does not advertise the {op:?} op").into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track_cmux(session: &mut Session, surface: &str) {
        match &mut session.backend {
            Backend::Cmux { tabs, .. } => tabs.push(Tab {
                tab: json!(surface),
                handle: surface.to_owned(),
            }),
            Backend::Tern { .. } => panic!("not a cmux session"),
        }
    }

    fn track_tern(session: &mut Session, block: u64) {
        match &mut session.backend {
            Backend::Tern { tabs, .. } => tabs.push(Tab {
                tab: json!(block),
                handle: block,
            }),
            Backend::Cmux { .. } => panic!("not a tern session"),
        }
    }

    fn env_with(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |key: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn cmux_is_selected_from_socket_env_and_respects_setting() {
        let env = env_with(&[("CMUX_SOCKET_PATH", "/tmp/cmux.sock")]);
        let config = BrowserConfig::default();
        assert_eq!(select(&config, &env).map(|s| s.label()), Some("cmux"));
        let disabled = BrowserConfig {
            cmux: false,
            tern: false,
            ..BrowserConfig::default()
        };
        assert!(select(&disabled, &env).is_none());
    }

    #[test]
    fn no_backend_without_environment() {
        let env = env_with(&[]);
        assert!(select(&BrowserConfig::default(), &env).is_none());
    }

    #[test]
    fn actions_require_an_open_surface_first() {
        let env = env_with(&[("CMUX_SOCKET_PATH", "/tmp/cmux-unreachable.sock")]);
        let mut session = select(&BrowserConfig::default(), &env).unwrap();
        let error = session
            .run("click", &json!({"selector": "#a"}))
            .unwrap_err()
            .into_message();
        assert!(error.contains("run open first"), "{error}");
    }

    #[test]
    fn unsupported_action_is_refused_by_name() {
        let env = env_with(&[("CMUX_SOCKET_PATH", "/tmp/cmux-unreachable.sock")]);
        let mut session = select(&BrowserConfig::default(), &env).unwrap();
        track_cmux(&mut session, "surface:1");
        let error = session
            .run("trace_start", &json!({}))
            .unwrap_err()
            .into_message();
        assert!(
            error.contains("not supported by the cmux backend"),
            "{error}"
        );
    }

    #[test]
    fn missing_tern_daemon_is_unavailable_so_open_can_fall_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.sock");
        let mut session = tern_session(path.to_str().unwrap());
        let error = session
            .run("open", &json!({"url": "about:blank"}))
            .unwrap_err();
        assert!(matches!(error, ExternalError::Unavailable(_)), "{error:?}");
    }

    #[cfg(unix)]
    fn read_frame(stream: &mut std::os::unix::net::UnixStream) -> Value {
        use std::io::Read;
        let mut header = [0u8; 4];
        stream.read_exact(&mut header).expect("frame header");
        let mut body = vec![0u8; u32::from_le_bytes(header) as usize];
        stream.read_exact(&mut body).expect("frame body");
        serde_json::from_slice(&body).expect("frame json")
    }

    #[cfg(unix)]
    fn write_frame(stream: &mut std::os::unix::net::UnixStream, message: &Value) {
        use std::io::Write;
        let body = serde_json::to_vec(message).expect("json");
        stream
            .write_all(&(body.len() as u32).to_le_bytes())
            .expect("header");
        stream.write_all(&body).expect("body");
    }

    fn tern_session(path: &str) -> Session {
        let path = path.to_owned();
        let env = move |key: &str| match key {
            "TERN_PANE_SOCKET" => Some(path.clone()),
            "TERN_PANE" => Some("1".to_owned()),
            _ => None,
        };
        let config = BrowserConfig {
            cmux: false,
            tern: true,
            ..BrowserConfig::default()
        };
        select(&config, env).expect("tern backend")
    }

    #[cfg(unix)]
    #[test]
    fn tern_nav_events_and_fork_round_trip_when_advertised() {
        use std::os::unix::net::UnixListener;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tern.sock").to_string_lossy().into_owned();
        let listener = UnixListener::bind(&path).expect("bind");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            assert_eq!(read_frame(&mut stream), json!({ "hello": {} }));
            write_frame(
                &mut stream,
                &json!({ "welcome": { "ops": ["browser", "nav", "events", "fork"] } }),
            );

            let nav = read_frame(&mut stream);
            assert_eq!(nav["browser"]["op"], "nav");
            assert_eq!(nav["browser"]["block"], 7);
            assert_eq!(nav["browser"]["go"], "back");
            let id = nav["id"].clone();
            write_frame(&mut stream, &json!({ "id": id, "browser": { "ok": null } }));

            let events = read_frame(&mut stream);
            assert_eq!(events["browser"]["op"], "events");
            assert_eq!(events["browser"]["after"], 5);
            let id = events["id"].clone();
            write_frame(
                &mut stream,
                &json!({ "id": id, "browser": { "ok": { "events": [{ "seq": 6 }] } } }),
            );

            let fork = read_frame(&mut stream);
            assert_eq!(fork["fork"]["block"], 1);
            assert_eq!(fork["fork"]["dir"], "right");
            let id = fork["id"].clone();
            write_frame(
                &mut stream,
                &json!({ "id": id, "fork": { "ok": { "block": 30 } } }),
            );
        });

        let mut session = tern_session(&path);
        track_tern(&mut session, 7);
        let back = session.run("back", &json!({})).expect("back");
        assert_eq!(back, json!({ "tab": 7, "action": "back" }));
        let events = session
            .run("events", &json!({ "after": 5 }))
            .expect("events");
        assert_eq!(events["events"], json!({ "events": [{ "seq": 6 }] }));
        let forked = session
            .run("fork", &json!({ "dir": "right" }))
            .expect("fork");
        assert_eq!(forked, json!({ "backend": "tern", "block": 30 }));
        assert!(
            session
                .run("fork", &json!({ "dir": "diagonal" }))
                .unwrap_err()
                .into_message()
                .contains("right or down")
        );
        server.join().expect("server");
    }

    #[cfg(unix)]
    #[test]
    fn tern_unadvertised_ops_are_refused_before_sending() {
        use std::os::unix::net::UnixListener;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tern.sock").to_string_lossy().into_owned();
        let listener = UnixListener::bind(&path).expect("bind");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            assert_eq!(read_frame(&mut stream), json!({ "hello": {} }));
            write_frame(&mut stream, &json!({ "welcome": { "ops": ["browser"] } }));
        });

        let mut session = tern_session(&path);
        track_tern(&mut session, 7);
        let error = session
            .run("reload", &json!({}))
            .unwrap_err()
            .into_message();
        assert!(
            error.contains("does not advertise the \"nav\" op"),
            "{error}"
        );
        let error = session.run("fork", &json!({})).unwrap_err().into_message();
        assert!(
            error.contains("does not advertise the \"fork\" op"),
            "{error}"
        );
        server.join().expect("server");
    }
}
