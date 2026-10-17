//! GitHub repository search as an MCP listing. A repository is not an install by itself: when
//! installing, its root `package.json` or `pyproject.toml` must name a package published on npm or
//! PyPI whose own metadata points back at the same repository, and that package must have one
//! clear command. Anything else is listed with its repository link to set up by hand.
//!
//! Endpoints used: `GET /search/repositories` and `GET /repos/<owner>/<repo>` on the API, and
//! `GET /<owner>/<repo>/<branch>/<file>` on the raw-content host. A token in `GITHUB_TOKEN` or
//! `GH_TOKEN` raises the anonymous search limit and is sent to `api.github.com` only.
use super::catalog::{Candidate, Endpoints, Install, Source, cache_env, normalize_repository};
use super::fetch::{self, pick_command, plain_name, plain_version};
use super::npm;
use super::smithery::safe_metadata;
use crate::config::types::McpServerConfig;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub const API_URL: &str = "https://api.github.com";
pub const RAW_URL: &str = "https://raw.githubusercontent.com";
/// Listing names are `github:<owner>/<repo>`.
pub const PREFIX: &str = "github:";
const PAGE_SIZE: usize = 20;
/// GitHub search returns at most this many results for one query.
const SEARCH_CAP: usize = 1000;

/// Candidates on page `page` (1-based) of a repository search, and whether another page follows.
/// An empty query browses repositories tagged `mcp-server`, most starred first.
pub fn search_page_at(base: &str, query: &str, page: usize) -> Result<(Vec<Candidate>, bool)> {
    let page = page.max(1);
    if (page - 1) * PAGE_SIZE >= SEARCH_CAP {
        return Ok((Vec::new(), false));
    }
    let mut url = url::Url::parse(&format!(
        "{}/search/repositories",
        base.trim_end_matches('/')
    ))
    .context("GitHub API URL is invalid")?;
    {
        let mut pairs = url.query_pairs_mut();
        match query.trim() {
            "" => {
                pairs
                    .append_pair("q", "topic:mcp-server archived:false")
                    .append_pair("sort", "stars")
                    .append_pair("order", "desc");
            }
            query => {
                pairs.append_pair("q", &format!("{query} mcp archived:false"));
            }
        }
        pairs
            .append_pair("per_page", &PAGE_SIZE.to_string())
            .append_pair("page", &page.to_string());
    }
    let reply = api_get(base, url.as_str(), "GitHub search")?;
    let body: Value = match reply.status {
        200..=299 => serde_json::from_str(&reply.body)
            .map_err(|error| anyhow!("GitHub search returned invalid JSON: {error}"))?,
        // Pages past the search cap are refused rather than empty.
        422 => return Ok((Vec::new(), false)),
        status => bail!("GitHub search returned status {status}"),
    };
    let items = body
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("GitHub search reply has no items array"))?;
    let total = body
        .get("total_count")
        .and_then(Value::as_u64)
        .map_or(0, |total| total as usize)
        .min(SEARCH_CAP);
    let more = !items.is_empty() && page * PAGE_SIZE < total;
    Ok((items.iter().filter_map(listing).collect(), more))
}

/// One search hit. Archived, disabled and malformed repositories are left out.
fn listing(item: &Value) -> Option<Candidate> {
    let flag = |key: &str| item.get(key).and_then(Value::as_bool) == Some(true);
    if flag("archived") || flag("disabled") {
        return None;
    }
    let full_name = safe_metadata(item.get("full_name"))?;
    let (_, repo) = split_repo(&full_name)?;
    let url = format!("https://github.com/{full_name}");
    let stars = item
        .get("stargazers_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some(Candidate {
        name: format!("{PREFIX}{full_name}"),
        title: repo.to_owned(),
        description: safe_metadata(item.get("description"))
            .unwrap_or_else(|| "No description".to_owned()),
        sources: vec![Source::Github],
        badges: vec![stars_badge(stars)],
        install: Install::Deferred(
            "looks for the repository's npm or PyPI package and its command".to_owned(),
        ),
        inputs: Vec::new(),
        repository: Some(url),
    })
}

/// The candidate to install for `owner/repo`. Network failures are errors; a repository with no
/// usable package is a candidate whose install is blocked with the reason.
pub fn resolve(endpoints: &Endpoints, full_name: &str, user_dir: &Path) -> Result<Candidate> {
    let (owner, repo) = split_repo(full_name)
        .ok_or_else(|| anyhow!("`{full_name}` is not an owner/repository name"))?;
    let api = endpoints.github_api.trim_end_matches('/');
    let reply = api_get(
        &endpoints.github_api,
        &format!("{api}/repos/{owner}/{repo}"),
        "GitHub",
    )?;
    let info: Value = match reply.status {
        200..=299 => serde_json::from_str(&reply.body)
            .map_err(|error| anyhow!("GitHub returned invalid JSON: {error}"))?,
        404 => bail!("github.com/{owner}/{repo} was not found"),
        status => bail!("GitHub returned status {status}"),
    };
    let mut candidate = listing(&info)
        .ok_or_else(|| anyhow!("github.com/{owner}/{repo} is archived or unavailable"))?;
    candidate.name = format!("{PREFIX}{full_name}");
    let url = format!("https://github.com/{owner}/{repo}");
    let branch = safe_metadata(info.get("default_branch"))
        .filter(|branch| valid_branch(branch))
        .ok_or_else(|| anyhow!("GitHub did not report a usable default branch for {url}"))?;
    let source = Repo {
        endpoints,
        owner,
        repo,
        branch: &branch,
        url: &url,
    };
    let found = [source.npm(user_dir)?, source.pypi(user_dir)?];
    candidate.install = found
        .iter()
        .flatten()
        .find(|install| matches!(install, Install::Server(_)))
        .or_else(|| found.iter().flatten().next())
        .cloned()
        .unwrap_or_else(|| {
            Install::Blocked(format!(
                "the repository has no root package.json or pyproject.toml naming a published package; follow its README to add it by hand: {url}"
            ))
        });
    Ok(candidate)
}

/// The repository being resolved. Each lookup is `Ok(None)` when the repository has no such
/// manifest, `Ok(Some(Blocked))` when it has one that cannot be used, and `Err` on network failure.
struct Repo<'a> {
    endpoints: &'a Endpoints,
    owner: &'a str,
    repo: &'a str,
    branch: &'a str,
    url: &'a str,
}

impl Repo<'_> {
    fn npm(&self, user_dir: &Path) -> Result<Option<Install>> {
        let Some(text) = self.file("package.json")? else {
            return Ok(None);
        };
        let Ok(package) = serde_json::from_str::<Value>(&text) else {
            return Ok(Some(blocked("its package.json is not valid JSON")));
        };
        // Monorepo roots are private; their servers live in subdirectories.
        if package.get("private").and_then(Value::as_bool) == Some(true) {
            return Ok(None);
        }
        let Some(name) = safe_metadata(package.get("name")).filter(|n| npm::valid_package(n))
        else {
            return Ok(None);
        };
        let Some(manifest) = npm::manifest(&self.endpoints.npm, &name)? else {
            return Ok(Some(blocked(format!(
                "its package.json names `{name}`, which is not published on npm"
            ))));
        };
        if !self.is_source(npm::repository(&manifest).as_deref()) {
            return Ok(Some(blocked(format!(
                "npm package `{name}` does not name this repository as its source"
            ))));
        }
        Ok(Some(match npm::from_manifest(&manifest, user_dir) {
            Some(candidate) => candidate.install,
            None => blocked(format!("npm package `{name}` declares no command to run")),
        }))
    }

    fn pypi(&self, user_dir: &Path) -> Result<Option<Install>> {
        let Some(text) = self.file("pyproject.toml")? else {
            return Ok(None);
        };
        let Ok(document) = text.parse::<toml::Table>() else {
            return Ok(Some(blocked("its pyproject.toml could not be parsed")));
        };
        let Some(project) = document.get("project").and_then(toml::Value::as_table) else {
            return Ok(None);
        };
        let Some(name) = project
            .get("name")
            .and_then(toml::Value::as_str)
            .filter(|name| plain_name(name))
        else {
            return Ok(None);
        };
        let scripts: Vec<String> = project
            .get("scripts")
            .and_then(toml::Value::as_table)
            .map(|scripts| {
                scripts
                    .keys()
                    .filter(|key| plain_name(key))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        if scripts.is_empty() {
            return Ok(Some(blocked(format!(
                "Python package `{name}` declares no command to run"
            ))));
        }
        let index = self.endpoints.pypi.trim_end_matches('/');
        let Some(release) = fetch::get_json(&format!("{index}/{name}/json"), &[], "PyPI")? else {
            return Ok(Some(blocked(format!(
                "its pyproject.toml names `{name}`, which is not published on PyPI"
            ))));
        };
        let info = release.get("info").unwrap_or(&Value::Null);
        let Some(version) = safe_metadata(info.get("version")).filter(|v| plain_version(v)) else {
            return Ok(Some(blocked(format!(
                "PyPI did not report a version for `{name}`"
            ))));
        };
        let listed = info
            .get("project_urls")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|urls| urls.values())
            .chain(info.get("home_page"))
            .filter_map(Value::as_str)
            .any(|url| self.is_source(normalize_repository(url).as_deref()));
        if !listed {
            return Ok(Some(blocked(format!(
                "PyPI package `{name}` does not name this repository as its source"
            ))));
        }
        // Only the package name counts: a CLI named after its repository usually needs a
        // subcommand to serve MCP, which the metadata does not say.
        Ok(Some(match pick_command(&scripts, &[name]) {
            Ok(script) => Install::Server(Box::new(McpServerConfig {
                program: PathBuf::from("uvx"),
                args: vec![
                    "--from".to_owned(),
                    format!("{name}=={version}"),
                    script.to_owned(),
                ],
                env: cache_env(user_dir),
                enabled: true,
                ..Default::default()
            })),
            Err(listed) => blocked(format!(
                "Python package `{name}` has several commands ({listed}) and none is clearly its MCP server; see {} for the command and add it by hand",
                self.url
            )),
        }))
    }

    /// A file at the repository root on its default branch, `None` when it does not exist.
    fn file(&self, path: &str) -> Result<Option<String>> {
        let raw = self.endpoints.github_raw.trim_end_matches('/');
        let url = format!("{raw}/{}/{}/{}/{path}", self.owner, self.repo, self.branch);
        let reply = fetch::get(&url, &[], "GitHub")?;
        match reply.status {
            200..=299 => Ok(Some(reply.body)),
            404 => Ok(None),
            status => bail!("GitHub returned status {status} for {path}"),
        }
    }

    /// Whether a package's declared source is this repository.
    fn is_source(&self, declared: Option<&str>) -> bool {
        declared.is_some_and(|declared| declared.eq_ignore_ascii_case(self.url))
    }
}

fn blocked(reason: impl Into<String>) -> Install {
    Install::Blocked(reason.into())
}

/// A GitHub API request. Rate limiting is reported as such, with how to raise the limit.
fn api_get(base: &str, url: &str, what: &str) -> Result<fetch::Reply> {
    let auth = token(base).map(|token| format!("Bearer {token}"));
    let mut headers = vec![
        ("Accept", "application/vnd.github+json"),
        ("X-GitHub-Api-Version", "2022-11-28"),
    ];
    if let Some(auth) = &auth {
        headers.push(("Authorization", auth.as_str()));
    }
    let reply = fetch::get(url, &headers, what)?;
    if matches!(reply.status, 403 | 429) {
        bail!(
            "{what} hit GitHub's rate limit{}",
            if auth.is_none() {
                "; set GITHUB_TOKEN or GH_TOKEN to raise it"
            } else {
                ""
            }
        );
    }
    Ok(reply)
}

/// The caller's GitHub token, offered to the real GitHub API only, never to another endpoint.
fn token(base: &str) -> Option<String> {
    if base.trim_end_matches('/') != API_URL {
        return None;
    }
    ["GITHUB_TOKEN", "GH_TOKEN"].iter().find_map(|key| {
        std::env::var(key)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    })
}

/// `owner/repo` with GitHub's name characters only.
fn split_repo(full_name: &str) -> Option<(&str, &str)> {
    let (owner, repo) = full_name.split_once('/')?;
    let part = |part: &str| {
        !part.is_empty()
            && part.len() <= 100
            && !part.starts_with('.')
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    (part(owner) && part(repo)).then_some((owner, repo))
}

fn valid_branch(branch: &str) -> bool {
    !branch.is_empty()
        && !branch.contains("..")
        && !branch.starts_with(['/', '-'])
        && branch
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

fn stars_badge(stars: u64) -> String {
    if stars >= 1000 {
        format!("{:.1}k stars", stars as f64 / 1000.0)
    } else {
        format!("{stars} stars")
    }
}

#[cfg(test)]
mod tests;
