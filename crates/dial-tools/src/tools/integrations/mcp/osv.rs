//! Known-malicious package check before an MCP server package first runs. OSV.dev publishes
//! malicious-package reports for npm and PyPI: OpenSSF `MAL-` records and GitHub advisories tagged
//! CWE-506 (embedded malicious code). A stdio server launched as an `npx`/`uvx` package with such a
//! report is refused before its package is downloaded or started. Ordinary vulnerability
//! advisories do not refuse a server.
//!
//! Best effort: only the package name and version are sent, and when OSV cannot be reached the
//! check passes, so installs keep working offline.
use super::catalog::launched_package;
use super::fetch;
use crate::config::types::McpServerConfig;
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::time::Duration;

pub(super) const URL: &str = "https://api.osv.dev/v1/query";
const TIMEOUT: Duration = Duration::from_secs(5);

/// Why `server` must not run, when OSV reports its package as malicious. `None` when it runs no
/// recognised package, OSV reports nothing malicious, or OSV could not be asked.
pub(super) fn refusal(url: &str, name: &str, server: &McpServerConfig) -> Option<String> {
    let package = launched_package(server)?;
    let ids = malicious_advisories(url, package.ecosystem, package.name, package.version).ok()?;
    let first = ids.first()?;
    Some(format!(
        "`{name}` runs {} package `{}`, which OSV.dev reports as malicious ({}); it was not started or saved. Details: https://osv.dev/vulnerability/{first}",
        package.ecosystem,
        package.name,
        ids.join(", ")
    ))
}

/// Ids of the advisories OSV lists for the package (at `version`, when given) that report it as
/// malicious. `Err` when OSV could not be asked or its reply could not be read.
pub(super) fn malicious_advisories(
    url: &str,
    ecosystem: &str,
    package: &str,
    version: Option<&str>,
) -> Result<Vec<String>> {
    let mut query = json!({"package": {"name": package, "ecosystem": ecosystem}});
    if let Some(version) = version {
        query["version"] = json!(version);
    }
    let mut response = fetch::agent(TIMEOUT)
        .post(url)
        .header("User-Agent", fetch::USER_AGENT)
        .send_json(&query)
        .map_err(|error| anyhow!("OSV query failed: {error}"))?;
    let status = response.status().as_u16();
    if !(200..=299).contains(&status) {
        bail!("OSV returned status {status}");
    }
    let body: Value = response
        .body_mut()
        .read_json()
        .map_err(|error| anyhow!("OSV reply could not be read: {error}"))?;
    Ok(body["vulns"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|vuln| reports_malware(vuln))
        .filter_map(|vuln| vuln["id"].as_str())
        .map(str::to_owned)
        .collect())
}

/// An OpenSSF malicious-package record (its id or an alias is `MAL-…`) or an advisory tagged
/// CWE-506, embedded malicious code.
fn reports_malware(vuln: &Value) -> bool {
    let mal = |id: &Value| id.as_str().is_some_and(|id| id.starts_with("MAL-"));
    mal(&vuln["id"])
        || vuln["aliases"]
            .as_array()
            .is_some_and(|ids| ids.iter().any(mal))
        || vuln["database_specific"]["cwe_ids"]
            .as_array()
            .is_some_and(|cwes| cwes.iter().any(|cwe| cwe == "CWE-506"))
}

#[cfg(test)]
mod tests;
