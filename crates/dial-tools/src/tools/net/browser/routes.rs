//! Browser network routes: parsing of `route`, `unroute` and `routes` arguments, glob matching, and the
//! routing decision. Precedence is fixed: the allowed-host guard runs first, so a request it refuses
//! is never offered to a user route. Only then do routes apply, first match in registration order.
//! Route bodies stay in the driver; the bridge sees only rule ids and kinds.

use serde_json::{Map, Value, json};

/// Most user routes one tab may hold.
pub const ROUTES_MAX: usize = 32;
/// Longest accepted route pattern, in bytes.
pub const PATTERN_MAX_BYTES: usize = 512;
/// Largest fulfilled response body, in bytes.
pub const FULFILL_BODY_MAX_BYTES: usize = 64 * 1024;
/// Most response headers a fulfilled route may set.
const FULFILL_HEADERS_MAX: usize = 32;

/// What a matched route does with the request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Abort,
    Continue,
    Fulfill,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Abort => "abort",
            Self::Continue => "continue",
            Self::Fulfill => "fulfill",
        }
    }

    fn parse(text: &str) -> Result<Self, String> {
        match text {
            "abort" => Ok(Self::Abort),
            "continue" => Ok(Self::Continue),
            "fulfill" => Ok(Self::Fulfill),
            other => Err(format!("unknown route kind {other:?}")),
        }
    }
}

/// A registered route as the bridge sees it: no bodies, only what decides a match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub id: u64,
    pub pattern: String,
    pub resource_type: Option<String>,
    pub kind: Kind,
}

/// Outcome of routing one request.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// No user route applies; the request proceeds normally.
    Continue,
    /// A route applies; the driver acts on `kind` using the stored response for `id`.
    Matched { id: u64, kind: Kind },
    /// The allowed-host guard refused the request; routes were not consulted.
    Blocked(String),
}

impl Rule {
    fn matches(&self, url: &str, resource_type: &str) -> bool {
        self.resource_type
            .as_deref()
            .is_none_or(|expected| expected == resource_type)
            && glob_matches(&self.pattern, url)
    }
}

/// Decide the fate of one request. When any rule exists the guard runs before any rule is consulted;
/// a refusal wins over every route. With no rules the guard is not called here, because the caller
/// performs the same guard on the ordinary path.
pub fn decide(
    url: &str,
    resource_type: &str,
    rules: &[Rule],
    guard: impl FnOnce() -> Result<(), String>,
) -> Decision {
    if rules.is_empty() {
        return Decision::Continue;
    }
    if let Err(reason) = guard() {
        return Decision::Blocked(reason);
    }
    rules
        .iter()
        .find(|rule| rule.matches(url, resource_type))
        .map_or(Decision::Continue, |rule| Decision::Matched {
            id: rule.id,
            kind: rule.kind,
        })
}

/// Read the rule list the driver attached to a bridge event.
pub fn rules_from_json(raw: &Value) -> Result<Vec<Rule>, String> {
    let entries = raw.as_array().ok_or("routes must be an array")?;
    if entries.len() > ROUTES_MAX {
        return Err(format!("a tab holds at most {ROUTES_MAX} routes"));
    }
    entries
        .iter()
        .map(|entry| {
            Ok(Rule {
                id: entry["id"].as_u64().ok_or("route id missing")?,
                pattern: entry["pattern"]
                    .as_str()
                    .ok_or("route pattern missing")?
                    .to_owned(),
                resource_type: entry["resource_type"].as_str().map(str::to_owned),
                kind: Kind::parse(entry["kind"].as_str().ok_or("route kind missing")?)?,
            })
        })
        .collect()
}

/// Parse `route`, `unroute` or `routes` arguments into the driver's request fields.
pub fn parse(action: &str, args: &Value) -> Result<Value, String> {
    match action {
        "route" => parse_route(args),
        "unroute" => Ok(json!({
            "pattern": match args.get("pattern") {
                None | Some(Value::Null) => Value::Null,
                Some(_) => json!(pattern(args)?),
            }
        })),
        "routes" => Ok(json!({})),
        _ => Err(format!("unsupported route action {action:?}")),
    }
}

fn pattern(args: &Value) -> Result<String, String> {
    let pattern = args
        .get("pattern")
        .and_then(Value::as_str)
        .ok_or("route requires pattern")?;
    if pattern.is_empty() || pattern.len() > PATTERN_MAX_BYTES {
        return Err(format!(
            "route pattern must be 1 through {PATTERN_MAX_BYTES} bytes"
        ));
    }
    if pattern.chars().any(char::is_control) {
        return Err("route pattern must not contain control characters".into());
    }
    Ok(pattern.to_owned())
}

const FULFILL_FIELDS: &[&str] = &["status", "headers", "content_type", "body"];

fn parse_route(args: &Value) -> Result<Value, String> {
    let pattern = pattern(args)?;
    let resource_type = match args.get("resource_type") {
        None | Some(Value::Null) => Value::Null,
        Some(Value::String(text))
            if !text.is_empty()
                && text.len() <= 32
                && text.bytes().all(|b| b.is_ascii_lowercase()) =>
        {
            json!(text)
        }
        Some(_) => {
            return Err(
                "route resource_type must be a lowercase resource name, such as xhr".into(),
            );
        }
    };
    let has_fulfill_fields = FULFILL_FIELDS.iter().any(|key| args.get(*key).is_some());
    let kind = match args.get("route_action").and_then(Value::as_str) {
        Some(text) => Kind::parse(text)?,
        None if has_fulfill_fields => Kind::Fulfill,
        None => Kind::Continue,
    };
    if kind != Kind::Fulfill && has_fulfill_fields {
        return Err("status, headers, content_type and body require route_action fulfill".into());
    }
    let response = match kind {
        Kind::Fulfill => fulfillment(args)?,
        Kind::Abort | Kind::Continue => Value::Null,
    };
    Ok(json!({
        "pattern": pattern,
        "resource_type": resource_type,
        "route_kind": match kind {
            Kind::Abort => "abort",
            Kind::Continue => "continue",
            Kind::Fulfill => "fulfill",
        },
        "response": response,
    }))
}

fn fulfillment(args: &Value) -> Result<Value, String> {
    let status = match args.get("status") {
        None => 200,
        Some(value) => value
            .as_u64()
            .filter(|status| (100..=599).contains(status))
            .ok_or("route status must be an integer from 100 through 599")?,
    };
    let headers = match args.get("headers") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(map)) if map.len() <= FULFILL_HEADERS_MAX => {
            let mut out = Map::new();
            for (name, value) in map {
                let text = value
                    .as_str()
                    .filter(|text| !text.contains(['\r', '\n']))
                    .ok_or("route headers must map names to single-line strings")?;
                if name.is_empty() || name.contains([' ', ':', '\r', '\n']) {
                    return Err(format!("invalid route header name {name:?}"));
                }
                out.insert(name.clone(), json!(text));
            }
            out
        }
        Some(_) => {
            return Err(format!(
                "route headers must be an object of up to {FULFILL_HEADERS_MAX} entries"
            ));
        }
    };
    let content_type = match args.get("content_type") {
        None | Some(Value::Null) => Value::Null,
        Some(Value::String(text)) if !text.contains(['\r', '\n']) && text.len() <= 256 => {
            json!(text)
        }
        Some(_) => {
            return Err("route content_type must be a single-line string up to 256 bytes".into());
        }
    };
    let (body, default_type) = match args.get("body") {
        None | Some(Value::Null) => (String::new(), None),
        Some(Value::String(text)) => (text.clone(), None),
        Some(structured) => (structured.to_string(), Some("application/json")),
    };
    if body.len() > FULFILL_BODY_MAX_BYTES {
        return Err(format!("route body exceeds {FULFILL_BODY_MAX_BYTES} bytes"));
    }
    let content_type = match content_type {
        Value::Null => default_type.map_or(Value::Null, |text| json!(text)),
        set => set,
    };
    Ok(json!({
        "status": status,
        "headers": headers,
        "content_type": content_type,
        "body": body,
    }))
}

/// Glob matching for route patterns: `*` matches within one path segment (no `/`), `**` matches
/// across segments, and every other character is literal. The pattern must match the whole URL.
pub fn glob_matches(glob: &str, url: &str) -> bool {
    #[derive(Clone, Copy)]
    enum Token {
        Literal(char),
        Segment,
        Any,
    }
    let mut tokens = Vec::new();
    let mut chars = glob.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '*' {
            if chars.peek() == Some(&'*') {
                chars.next();
                tokens.push(Token::Any);
            } else {
                tokens.push(Token::Segment);
            }
        } else {
            tokens.push(Token::Literal(c));
        }
    }
    let text: Vec<char> = url.chars().collect();
    // reachable[i]: the tokens consumed so far can match exactly text[..i].
    let mut reachable = vec![false; text.len() + 1];
    reachable[0] = true;
    for token in tokens {
        let mut next = vec![false; text.len() + 1];
        match token {
            Token::Literal(expected) => {
                for i in 0..text.len() {
                    next[i + 1] = reachable[i] && text[i] == expected;
                }
            }
            Token::Segment => {
                for j in 0..=text.len() {
                    next[j] = reachable[j] || (j > 0 && next[j - 1] && text[j - 1] != '/');
                }
            }
            Token::Any => {
                for j in 0..=text.len() {
                    next[j] = reachable[j] || (j > 0 && next[j - 1]);
                }
            }
        }
        reachable = next;
    }
    reachable[text.len()]
}

#[cfg(test)]
mod tests;
