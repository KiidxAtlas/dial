//! Bounded OpenAPI path/method/status matching; source strings never leave this module.
use anyhow::{Context, Result, ensure};
use regex::{Regex, RegexBuilder};
use serde_json::{Value, json};
use url::Url;

pub(super) const MAX_PATHS: usize = 2000;
pub(super) const MAX_TEMPLATE_REGEX_BYTES: usize = 64 * 1024;
pub(super) struct Spec {
    paths: Vec<Route>,
}
struct Route {
    path: String,
    item: Value,
    template: Option<Regex>,
}
impl Spec {
    pub(super) fn parse(value: Value) -> Result<Self> {
        ensure!(
            value
                .get("openapi")
                .and_then(Value::as_str)
                .is_some_and(|v| v.starts_with("3.")),
            "requires OpenAPI 3 JSON"
        );
        let paths = value
            .get("paths")
            .and_then(Value::as_object)
            .context("OpenAPI paths must be an object")?;
        // Specification extensions are not routes, even when their values resemble path items.
        let paths: Vec<_> = paths
            .iter()
            .filter(|(path, _)| !path.starts_with("x-"))
            .collect();
        ensure!(paths.len() <= MAX_PATHS, "OpenAPI exceeds 2000 path limit");
        let paths = paths
            .into_iter()
            .map(|(path, item)| {
                ensure!(
                    path.starts_with('/') && path.len() <= 4096,
                    "invalid or oversized OpenAPI path"
                );
                Ok(Route {
                    path: path.clone(),
                    item: item.clone(),
                    template: compile_template(path)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { paths })
    }
    pub(super) fn compare(&self, url: Option<&Url>, method: &str, status: Option<u64>) -> Value {
        let Some(url) = url else {
            return json!({"state":"url_unavailable"});
        };
        let exact = self.paths.iter().position(|route| route.path == url.path());
        let candidates: Vec<_> = exact.map(|i| vec![i]).unwrap_or_else(|| {
            self.paths
                .iter()
                .enumerate()
                .filter(|(_, route)| {
                    route
                        .template
                        .as_ref()
                        .is_some_and(|matcher| matcher.is_match(url.path()))
                })
                .map(|(i, _)| i)
                .collect()
        });
        let [index] = candidates.as_slice() else {
            return json!({"state":if candidates.is_empty() {"path_not_declared"} else {"ambiguous_template"},"candidate_count":candidates.len()});
        };
        let item = &self.paths[*index].item;
        let basis = if exact.is_some() {
            "literal_path"
        } else {
            "path_template_only"
        };
        let base = |state| json!({"state":state,"route_id":index+1,"path_match":basis});
        if item.get("$ref").is_some() {
            return base("path_ref_not_resolved");
        }
        let Some(operation) = item.get(method.to_ascii_lowercase()) else {
            return base("method_not_declared");
        };
        if operation.get("$ref").is_some() {
            return base("operation_ref_not_resolved");
        }
        let Some(responses) = operation.get("responses").and_then(Value::as_object) else {
            return base("responses_unavailable");
        };
        let Some(status @ 100..=599) = status else {
            return base("status_unavailable");
        };
        let declaration = if responses.contains_key(&status.to_string()) {
            Some("exact")
        } else if responses.contains_key(&format!("{}XX", status / 100)) {
            Some("range")
        } else if responses.contains_key("default") {
            Some("default")
        } else {
            None
        };
        let mut result = base(if declaration.is_some() {
            "status_declared"
        } else {
            "status_not_declared"
        });
        result["status_declaration"] = json!(declaration);
        result
    }
}

// Compile once per source, not per request. Escaped literals cannot inject regex syntax, and
// nonempty placeholders cannot consume '/'. The regex engine avoids recursive backtracking.
fn compile_template(path: &str) -> Result<Option<Regex>> {
    if !path.contains(['{', '}']) {
        return Ok(None);
    }
    let mut remaining = path;
    let mut pattern = String::from("\\A");
    while let Some(open) = remaining.find('{') {
        let literal = &remaining[..open];
        ensure!(!literal.contains('}'), "invalid OpenAPI path template");
        pattern.push_str(&regex::escape(literal));
        remaining = &remaining[open + 1..];
        let close = remaining
            .find('}')
            .context("invalid OpenAPI path template")?;
        let name = &remaining[..close];
        ensure!(
            !name.is_empty() && !name.contains(['{', '/']),
            "invalid OpenAPI path template"
        );
        pattern.push_str("[^/]+");
        remaining = &remaining[close + 1..];
    }
    ensure!(!remaining.contains('}'), "invalid OpenAPI path template");
    pattern.push_str(&regex::escape(remaining));
    pattern.push_str("\\z");
    let matcher = RegexBuilder::new(&pattern)
        .size_limit(MAX_TEMPLATE_REGEX_BYTES)
        .build()
        .map_err(|_| {
            anyhow::anyhow!("OpenAPI path template exceeds 64 KiB compiled matcher limit")
        })?;
    Ok(Some(matcher))
}
