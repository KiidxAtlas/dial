//! Host discovery from OpenSSH client configuration (`~/.ssh/config` plus its `Include`s).
//!
//! Only concrete `Host` aliases (no `*`, `?` or `!` patterns) are listed; `Match` blocks are not
//! evaluated. Discovery is for naming hosts: connections still hand the alias to OpenSSH, which
//! applies the full configuration itself.

use globset::GlobBuilder;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// `Include` nesting OpenSSH itself allows.
const MAX_INCLUDE_DEPTH: usize = 16;
/// Ignore absurdly large configuration files instead of reading them whole.
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiscoveredHost {
    pub alias: String,
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_file: Option<String>,
    pub source: PathBuf,
}

/// Concrete aliases from `<home>/.ssh/config`, first definition wins.
pub fn discover(home: &Path) -> Vec<DiscoveredHost> {
    let ssh_dir = home.join(".ssh");
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    parse_file(
        &ssh_dir.join("config"),
        home,
        &ssh_dir,
        0,
        &mut seen,
        &mut out,
    );
    out
}

/// Every host Dial can address: the `~/.ssh/config` aliases merged with the `[ssh_hosts]` table.
/// A configured entry wins over a discovered alias with the same name; configured entries have an
/// empty `source` because they come from Dial's configuration, not an OpenSSH file.
pub fn resolve(
    home: &Path,
    configured: &std::collections::BTreeMap<String, crate::config::types::SshHostConfig>,
) -> Vec<DiscoveredHost> {
    let mut hosts = discover(home);
    for (alias, entry) in configured {
        let host = DiscoveredHost {
            alias: alias.clone(),
            hostname: Some(entry.host.clone()),
            user: entry.user.clone(),
            port: entry.port,
            identity_file: entry.key.clone(),
            source: PathBuf::new(),
        };
        match hosts.iter_mut().find(|known| known.alias == *alias) {
            Some(known) => *known = host,
            None => hosts.push(host),
        }
    }
    hosts
}

fn parse_file(
    path: &Path,
    home: &Path,
    ssh_dir: &Path,
    depth: usize,
    seen: &mut BTreeSet<PathBuf>,
    out: &mut Vec<DiscoveredHost>,
) {
    if depth > MAX_INCLUDE_DEPTH || !seen.insert(path.to_path_buf()) {
        return;
    }
    let Ok(meta) = fs::metadata(path) else { return };
    if !meta.is_file() || meta.len() > MAX_CONFIG_BYTES {
        return;
    }
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    parse(&text, path, home, ssh_dir, depth, seen, out);
}

/// Parse one configuration text, appending newly named hosts to `out`.
pub fn parse(
    text: &str,
    source: &Path,
    home: &Path,
    ssh_dir: &Path,
    depth: usize,
    seen: &mut BTreeSet<PathBuf>,
    out: &mut Vec<DiscoveredHost>,
) {
    // Indices into `out` the current `Host` block configures.
    let mut current: Vec<usize> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((keyword, args)) = split_keyword(line) else {
            continue;
        };
        let words = tokenize(args);
        match keyword.to_ascii_lowercase().as_str() {
            "host" => {
                current.clear();
                for pattern in &words {
                    if pattern.contains(['*', '?', '!']) || pattern.is_empty() {
                        continue;
                    }
                    match out.iter().position(|host| &host.alias == pattern) {
                        // A later block for an already-known alias can still supply missing fields.
                        Some(index) => current.push(index),
                        None => {
                            out.push(DiscoveredHost {
                                alias: pattern.clone(),
                                source: source.to_path_buf(),
                                ..DiscoveredHost::default()
                            });
                            current.push(out.len() - 1);
                        }
                    }
                }
            }
            "match" => current.clear(),
            "include" => {
                for pattern in &words {
                    for file in expand_include(pattern, home, ssh_dir) {
                        parse_file(&file, home, ssh_dir, depth + 1, seen, out);
                    }
                }
            }
            key @ ("hostname" | "user" | "port" | "identityfile") => {
                let Some(value) = words.first() else { continue };
                for &index in &current {
                    let host = &mut out[index];
                    match key {
                        "hostname" if host.hostname.is_none() => {
                            host.hostname = Some(expand_tokens(value, &host.alias));
                        }
                        "user" if host.user.is_none() => host.user = Some(value.clone()),
                        "port" if host.port.is_none() => {
                            host.port = value.parse().ok().filter(|p| *p > 0)
                        }
                        "identityfile" if host.identity_file.is_none() => {
                            host.identity_file = Some(value.clone());
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

/// `Keyword args`, `Keyword=args` or `Keyword = args`.
fn split_keyword(line: &str) -> Option<(&str, &str)> {
    let end = line.find(|c: char| c.is_whitespace() || c == '=')?;
    let keyword = &line[..end];
    let mut rest = line[end..].trim_start();
    if let Some(after) = rest.strip_prefix('=') {
        rest = after.trim_start();
    }
    (!keyword.is_empty()).then_some((keyword, rest))
}

/// Whitespace-separated arguments with double-quoted groups.
fn tokenize(args: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut started = false;
    for c in args.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '#' if !quoted && !started => break,
            c if c.is_whitespace() && !quoted => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(word);
    }
    words
}

/// `%h` (the alias) and `%%` in `HostName`.
fn expand_tokens(value: &str, alias: &str) -> String {
    let mut out = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('h') => out.push_str(alias),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

/// Files an `Include` argument names: `~` is the home directory, relative paths are under
/// `~/.ssh`, and glob characters in the final component expand in sorted order.
fn expand_include(pattern: &str, home: &Path, ssh_dir: &Path) -> Vec<PathBuf> {
    let path = if let Some(rest) = pattern.strip_prefix("~/") {
        home.join(rest)
    } else if Path::new(pattern).is_absolute() {
        PathBuf::from(pattern)
    } else {
        ssh_dir.join(pattern)
    };
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return Vec::new();
    };
    if !name.contains(['*', '?', '[']) {
        return vec![path];
    }
    let Some(parent) = path.parent() else {
        return Vec::new();
    };
    if parent.to_string_lossy().contains(['*', '?', '[']) {
        // Directory globs are rare in SSH configs; only the last component is expanded.
        return Vec::new();
    }
    let Ok(glob) = GlobBuilder::new(name).literal_separator(true).build() else {
        return Vec::new();
    };
    let matcher = glob.compile_matcher();
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| matcher.is_match(entry.file_name()))
        .map(|entry| entry.path())
        .collect();
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_text(text: &str) -> Vec<DiscoveredHost> {
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        let dir = Path::new("/nonexistent/.ssh");
        parse(
            text,
            &dir.join("config"),
            Path::new("/nonexistent"),
            dir,
            0,
            &mut seen,
            &mut out,
        );
        out
    }

    #[test]
    fn lists_concrete_aliases_with_first_values() {
        let hosts = parse_text(
            "# comment\n\
             Host prod staging *.internal !bad\n\
             \tHostName %h.example.com\n\
             \tUser deploy\n\
             \tPort 2222\n\
             \tIdentityFile ~/.ssh/id_prod\n\
             Host prod\n\
             \tUser ignored\n\
             \tProxyJump bastion\n\
             Host=bastion\n\
             HostName = 10.0.0.1\n\
             Host *\n\
             \tUser everyone\n",
        );
        let names: Vec<_> = hosts.iter().map(|h| h.alias.as_str()).collect();
        assert_eq!(names, ["prod", "staging", "bastion"]);
        assert_eq!(hosts[0].hostname.as_deref(), Some("prod.example.com"));
        assert_eq!(hosts[1].hostname.as_deref(), Some("staging.example.com"));
        assert_eq!(hosts[0].user.as_deref(), Some("deploy"));
        assert_eq!(hosts[0].port, Some(2222));
        assert_eq!(hosts[0].identity_file.as_deref(), Some("~/.ssh/id_prod"));
        assert_eq!(hosts[2].hostname.as_deref(), Some("10.0.0.1"));
        assert_eq!(hosts[2].user, None);
    }

    #[test]
    fn match_blocks_and_quotes() {
        let hosts = parse_text(
            "Host \"quoted name\" plain # trailing comment\n\
             Match host plain\n\
             \tUser matched\n\
             Host plain\n\
             \tPort notanumber\n",
        );
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts[0].alias, "quoted name");
        assert_eq!(hosts[1].user, None);
        assert_eq!(hosts[1].port, None);
    }

    #[test]
    fn follows_includes_relative_to_ssh_dir_with_globs() {
        let home = tempfile::tempdir().unwrap();
        let ssh = home.path().join(".ssh");
        fs::create_dir_all(ssh.join("conf.d")).unwrap();
        fs::write(
            ssh.join("config"),
            "Include conf.d/*.conf\nHost main\n  User me\nInclude config\n",
        )
        .unwrap();
        fs::write(ssh.join("conf.d/b.conf"), "Host beta\n  Port 2200\n").unwrap();
        fs::write(
            ssh.join("conf.d/a.conf"),
            "Host alpha\nInclude ~/.ssh/extra\n",
        )
        .unwrap();
        fs::write(ssh.join("conf.d/skip.txt"), "Host skipped\n").unwrap();
        fs::write(ssh.join("extra"), "Host gamma\n").unwrap();
        let hosts = discover(home.path());
        let names: Vec<_> = hosts.iter().map(|h| h.alias.as_str()).collect();
        assert_eq!(names, ["alpha", "gamma", "beta", "main"]);
        assert_eq!(hosts[2].port, Some(2200));
        assert_eq!(hosts[3].user.as_deref(), Some("me"));
        assert!(hosts[2].source.ends_with("conf.d/b.conf"));
    }

    #[test]
    fn missing_config_discovers_nothing() {
        let home = tempfile::tempdir().unwrap();
        assert!(discover(home.path()).is_empty());
    }

    #[test]
    fn configured_hosts_merge_and_win_on_alias_clash() {
        let home = tempfile::tempdir().unwrap();
        let ssh = home.path().join(".ssh");
        fs::create_dir_all(&ssh).unwrap();
        fs::write(ssh.join("config"), "Host alpha\n  Port 2200\nHost beta\n").unwrap();
        let mut configured = std::collections::BTreeMap::new();
        configured.insert(
            "alpha".to_owned(),
            crate::config::types::SshHostConfig {
                host: "10.0.0.5".into(),
                user: Some("deploy".into()),
                port: Some(22),
                key: None,
            },
        );
        configured.insert(
            "gamma".to_owned(),
            crate::config::types::SshHostConfig {
                host: "gamma.example".into(),
                user: None,
                port: None,
                key: Some("/keys/g".into()),
            },
        );
        let hosts = resolve(home.path(), &configured);
        let names: Vec<_> = hosts.iter().map(|h| h.alias.as_str()).collect();
        assert_eq!(names, ["alpha", "beta", "gamma"]);
        assert_eq!(hosts[0].hostname.as_deref(), Some("10.0.0.5"));
        assert_eq!(hosts[0].port, Some(22));
        assert_eq!(hosts[0].user.as_deref(), Some("deploy"));
        assert_eq!(hosts[2].identity_file.as_deref(), Some("/keys/g"));
        assert!(hosts[2].source.as_os_str().is_empty());
    }
}
