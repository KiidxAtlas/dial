//! `domains` and `exclude_domains` for `web_search`. A site matches a result when the result's host is
//! the site's host or one of its subdomains and, if the site names a path, the result's path lies
//! under that path. Every provider's results pass through [`DomainFilter::apply`]; a provider's native
//! filter (see [`DomainFilter::native_include`] and [`DomainFilter::native_exclude`]) only narrows the
//! request early and never replaces this check.

use super::super::search::SearchResult;

/// The most entries one of the two lists may hold.
pub const MAX_DOMAINS: usize = 20;

/// One site: a lowercase host and an optional lowercase path prefix without a trailing slash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    host: String,
    path: String,
}

impl Site {
    fn parse(field: &str, entry: &str) -> Result<Self, String> {
        let trimmed = entry.trim();
        let invalid = || {
            format!("{field} entry {trimmed:?} is not a hostname, optionally followed by a path")
        };
        if trimmed.is_empty() || trimmed.contains("://") || trimmed.chars().any(char::is_whitespace)
        {
            return Err(invalid());
        }
        let parsed = url::Url::parse(&format!("https://{trimmed}")).map_err(|_| invalid())?;
        if parsed.port().is_some()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(invalid());
        }
        let host = parsed
            .host_str()
            .filter(|host| !host.is_empty())
            .ok_or_else(invalid)?
            .to_ascii_lowercase();
        let path = parsed.path().trim_end_matches('/').to_ascii_lowercase();
        Ok(Self { host, path })
    }

    /// Whether this site has no path, so a provider can filter it by host alone.
    fn is_whole_host(&self) -> bool {
        self.path.is_empty()
    }

    fn matches(&self, result_url: &str) -> bool {
        let Ok(parsed) = url::Url::parse(result_url) else {
            return false;
        };
        let Some(host) = parsed.host_str().map(str::to_ascii_lowercase) else {
            return false;
        };
        let host_matches = host == self.host
            || host
                .strip_suffix(self.host.as_str())
                .is_some_and(|rest| rest.ends_with('.'));
        if !host_matches {
            return false;
        }
        if self.path.is_empty() {
            return true;
        }
        let path = parsed.path().trim_end_matches('/').to_ascii_lowercase();
        path == self.path || path.starts_with(&format!("{}/", self.path))
    }

    fn display(&self) -> String {
        format!("{}{}", self.host, self.path)
    }
}

fn parse_list(field: &str, entries: &[String]) -> Result<Vec<Site>, String> {
    if entries.len() > MAX_DOMAINS {
        return Err(format!("{field} accepts at most {MAX_DOMAINS} entries"));
    }
    entries
        .iter()
        .map(|entry| Site::parse(field, entry))
        .collect()
}

/// Both domain lists of one search. The default has no constraints.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DomainFilter {
    include: Vec<Site>,
    exclude: Vec<Site>,
}

/// The filter that constrains nothing, for callers that need a `'static` one (tests).
#[cfg(test)]
pub static UNFILTERED: DomainFilter = DomainFilter {
    include: Vec::new(),
    exclude: Vec::new(),
};

impl DomainFilter {
    /// Validates both lists; an entry must be a hostname with an optional path, never a URL.
    pub fn new(include: &[String], exclude: &[String]) -> Result<Self, String> {
        Ok(Self {
            include: parse_list("domains", include)?,
            exclude: parse_list("exclude_domains", exclude)?,
        })
    }

    /// The include hosts a provider may filter natively. `None` when the list is empty or any entry
    /// names a path, because a host-level filter would drop results that the path entry keeps.
    pub fn native_include(&self) -> Option<Vec<&str>> {
        if self.include.is_empty() {
            return None;
        }
        self.include
            .iter()
            .map(|site| site.is_whole_host().then_some(site.host.as_str()))
            .collect()
    }

    /// The exclude hosts a provider may filter natively. Path entries are left to [`Self::apply`],
    /// since a host-level exclusion would also drop the rest of that host.
    pub fn native_exclude(&self) -> Vec<&str> {
        self.exclude
            .iter()
            .filter(|site| site.is_whole_host())
            .map(|site| site.host.as_str())
            .collect()
    }

    /// Applies both lists to `results`. A list that would remove every result is not applied and is
    /// reported in the returned note instead, so the search still answers.
    pub fn apply(&self, results: Vec<SearchResult>) -> (Vec<SearchResult>, Option<String>) {
        let mut results = results;
        let mut notes = Vec::new();
        if !self.include.is_empty() {
            let kept = results
                .iter()
                .filter(|result| self.include.iter().any(|site| site.matches(&result.url)))
                .count();
            if kept == 0 && !results.is_empty() {
                notes.push(format!(
                    "no result came from {}, so the results are not limited to those sites",
                    names(&self.include)
                ));
            } else {
                results.retain(|result| self.include.iter().any(|site| site.matches(&result.url)));
            }
        }
        if !self.exclude.is_empty() {
            let kept = results
                .iter()
                .filter(|result| !self.exclude.iter().any(|site| site.matches(&result.url)))
                .count();
            if kept == 0 && !results.is_empty() {
                notes.push(format!(
                    "every result came from {}, so the exclusion was not applied",
                    names(&self.exclude)
                ));
            } else {
                results.retain(|result| !self.exclude.iter().any(|site| site.matches(&result.url)));
            }
        }
        (results, (!notes.is_empty()).then(|| notes.join("; ")))
    }
}

fn names(sites: &[Site]) -> String {
    sites
        .iter()
        .map(Site::display)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests;
