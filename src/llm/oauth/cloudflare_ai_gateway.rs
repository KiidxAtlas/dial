//! Cloudflare AI Gateway sign-in. Not a browser OAuth grant: the user pastes an AI Gateway token
//! plus the account and gateway identifiers. The token is sent as the Anthropic key on the gateway's
//! `/anthropic` route, so no extra headers are required.

use super::super::model_config::ApiProtocol;
use super::{Access, Credentials, Flow, Login, util};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::sync::atomic::Ordering;

pub const FLOW: Flow = Flow {
    id: "cloudflare-ai-gateway",
    name: "Cloudflare AI Gateway",
    base_url: "https://gateway.ai.cloudflare.com/v1/<account>/<gateway>/anthropic",
    protocol: ApiProtocol::Anthropic,
    needs_terminal: true,
    subscription: false,
    login,
    refresh: None,
    access,
};

const AUTH_URL: &str = "https://developers.cloudflare.com/ai-gateway/configuration/authentication/";
const AUTH_INSTRUCTIONS: &str =
    "Create an AI Gateway token with Run permission, then copy it here.";
const ACCOUNT_KEY: &str = "accountId";
const GATEWAY_KEY: &str = "gatewayId";

fn login(l: &Login<'_>) -> Result<Credentials> {
    l.ui.auth(AUTH_URL, Some(AUTH_INSTRUCTIONS));
    let token = prompt_required(
        l,
        "Paste your Cloudflare AI Gateway token/API key",
        "cfut_...",
        "Cloudflare AI Gateway token is required",
    )?;
    let account = prompt_required(
        l,
        "Enter your Cloudflare account ID",
        "32-character account ID",
        "Cloudflare account ID is required",
    )?;
    let gateway = prompt_required(
        l,
        "Enter your Cloudflare AI Gateway ID",
        "default",
        "Cloudflare AI Gateway ID is required",
    )?;
    let mut extra = serde_json::Map::new();
    extra.insert(ACCOUNT_KEY.into(), Value::String(account));
    extra.insert(GATEWAY_KEY.into(), Value::String(gateway));
    Ok(Credentials {
        access: token,
        expires_at_ms: util::NEVER_EXPIRES,
        extra,
        ..Credentials::default()
    })
}

fn prompt_required(
    l: &Login<'_>,
    message: &str,
    placeholder: &str,
    missing: &str,
) -> Result<String> {
    let value = l.ui.prompt(message, Some(placeholder), true)?;
    ensure!(!l.cancel.load(Ordering::SeqCst), "sign-in cancelled");
    let value = value.trim();
    ensure!(!value.is_empty(), "{missing}");
    Ok(value.to_owned())
}

fn access(c: &Credentials) -> Result<Access> {
    let account = c
        .extra_str(ACCOUNT_KEY)
        .context("Cloudflare AI Gateway credentials are missing the account ID; run /login cloudflare-ai-gateway again")?;
    let gateway = c
        .extra_str(GATEWAY_KEY)
        .context("Cloudflare AI Gateway credentials are missing the gateway ID; run /login cloudflare-ai-gateway again")?;
    ensure!(
        is_path_segment(account) && is_path_segment(gateway),
        "Cloudflare AI Gateway account or gateway ID is invalid; run /login cloudflare-ai-gateway again"
    );
    let mut access = super::bearer(c)?;
    access.base_url = Some(format!(
        "https://gateway.ai.cloudflare.com/v1/{account}/{gateway}/anthropic"
    ));
    Ok(access)
}

/// Identifiers are interpolated into a URL path, so they must be a single non-empty segment.
fn is_path_segment(value: &str) -> bool {
    !value.is_empty() && !value.contains(['/', '?', '#']) && !value.chars().any(char::is_whitespace)
}

#[cfg(test)]
mod tests {
    use super::super::Interaction;
    use super::*;
    use parking_lot::Mutex;
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    /// Scripted prompts; records each prompt message in order.
    struct Scripted {
        answers: Mutex<VecDeque<String>>,
        prompts: Mutex<Vec<String>>,
    }

    impl Scripted {
        fn new(answers: &[&str]) -> Self {
            Self {
                answers: Mutex::new(answers.iter().map(|s| s.to_string()).collect()),
                prompts: Mutex::new(Vec::new()),
            }
        }
    }

    impl Interaction for Scripted {
        fn auth(&self, _url: &str, _instructions: Option<&str>) {}
        fn progress(&self, _message: &str) {}
        fn prompt(
            &self,
            message: &str,
            _placeholder: Option<&str>,
            _allow_empty: bool,
        ) -> Result<String> {
            self.prompts.lock().push(message.to_owned());
            Ok(self.answers.lock().pop_front().unwrap_or_default())
        }
    }

    fn run_login(ui: &Scripted) -> Result<Credentials> {
        let login = Login {
            ui,
            cancel: Arc::new(AtomicBool::new(false)),
        };
        super::login(&login)
    }

    fn credentials(account: &str, gateway: &str) -> Credentials {
        let mut extra = serde_json::Map::new();
        extra.insert(ACCOUNT_KEY.into(), Value::String(account.into()));
        extra.insert(GATEWAY_KEY.into(), Value::String(gateway.into()));
        Credentials {
            access: "cfut_secret".into(),
            expires_at_ms: util::NEVER_EXPIRES,
            extra,
            ..Credentials::default()
        }
    }

    #[test]
    fn flow_invariants() {
        assert_eq!(FLOW.id, "cloudflare-ai-gateway");
        assert_eq!(FLOW.name, "Cloudflare AI Gateway");
        assert!(matches!(FLOW.protocol, ApiProtocol::Anthropic));
        assert!(FLOW.needs_terminal);
        assert!(!FLOW.subscription);
        assert!(FLOW.refresh.is_none());
        assert!(FLOW.base_url.contains("<account>") && FLOW.base_url.ends_with("/anthropic"));
    }

    #[test]
    fn login_collects_token_account_and_gateway_in_order() {
        let ui = Scripted::new(&["  cfut_abc  ", " acct123 ", " default "]);
        let creds = run_login(&ui).unwrap();
        assert_eq!(creds.access, "cfut_abc");
        assert!(creds.refresh.is_empty());
        assert_eq!(creds.expires_at_ms, util::NEVER_EXPIRES);
        assert_eq!(creds.extra_str(ACCOUNT_KEY), Some("acct123"));
        assert_eq!(creds.extra_str(GATEWAY_KEY), Some("default"));
        let prompts = ui.prompts.lock().clone();
        assert_eq!(
            prompts,
            vec![
                "Paste your Cloudflare AI Gateway token/API key",
                "Enter your Cloudflare account ID",
                "Enter your Cloudflare AI Gateway ID",
            ]
        );
    }

    #[test]
    fn login_rejects_empty_inputs_with_specific_messages() {
        let cases = [
            (
                ["", "acct", "gw"],
                "Cloudflare AI Gateway token is required",
            ),
            (["tok", "  ", "gw"], "Cloudflare account ID is required"),
            (["tok", "acct", ""], "Cloudflare AI Gateway ID is required"),
        ];
        for (answers, expected) in cases {
            let ui = Scripted::new(&answers);
            let err = run_login(&ui).err().unwrap();
            assert_eq!(err.to_string(), expected);
        }
    }

    #[test]
    fn access_substitutes_account_and_gateway_into_anthropic_route() {
        let access = access(&credentials("acct123", "default")).unwrap();
        assert_eq!(access.token.as_str(), "cfut_secret");
        assert_eq!(
            access.base_url.as_deref(),
            Some("https://gateway.ai.cloudflare.com/v1/acct123/default/anthropic")
        );
        assert!(access.headers.is_empty());
    }

    #[test]
    fn access_rejects_missing_or_unsafe_identifiers() {
        let missing = Credentials {
            access: "cfut_secret".into(),
            ..Credentials::default()
        };
        assert!(access(&missing).is_err());
        assert!(access(&credentials("acct/evil", "default")).is_err());
        assert!(access(&credentials("acct", "gw?x=1")).is_err());
        assert!(access(&credentials("acct", "my gw")).is_err());
    }
}
