use super::*;

fn track_cmux(session: &mut Session, surface: &str) {
    session.tabs.push(Tab {
        tab: json!(surface),
        handle: surface.to_owned(),
    });
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
        .unwrap_err();
    assert!(error.contains("run open first"), "{error}");
}

#[test]
fn unsupported_action_is_refused_by_name() {
    let env = env_with(&[("CMUX_SOCKET_PATH", "/tmp/cmux-unreachable.sock")]);
    let mut session = select(&BrowserConfig::default(), &env).unwrap();
    track_cmux(&mut session, "surface:1");
    let error = session.run("trace_start", &json!({})).unwrap_err();
    assert!(
        error.contains("not supported by the cmux backend"),
        "{error}"
    );
}
