//! `ssh://[user@]host[:port]/path` parsing and POSIX quoting for remote commands.
//!
//! The path is taken verbatim (no `.`/`..` normalisation) after strict percent-decoding, so what
//! the model wrote is exactly what the remote shell sees. `?` and `#` are rejected rather than
//! silently truncating the path; a literal one must be written `%3F`/`%23`. `ssh://host/~/x` is
//! relative to the remote `$HOME`.

/// Where a remote file lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemotePath {
    /// An absolute path such as `/etc/hosts` (always starts with `/`).
    Absolute(String),
    /// Relative to the remote `$HOME`; empty means the home directory itself.
    Home(String),
}

impl RemotePath {
    /// One POSIX shell word that expands to this path on the remote side.
    pub fn shell_word(&self) -> String {
        match self {
            RemotePath::Absolute(path) => sh_quote(path),
            RemotePath::Home(rest) if rest.is_empty() => "\"$HOME\"".to_owned(),
            RemotePath::Home(rest) => format!("\"$HOME\"/{}", sh_quote(rest)),
        }
    }
}

/// A parsed `ssh://` URL that names a host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshUrl {
    /// The URL as written, without any read selector.
    pub href: String,
    /// Decoded authority exactly as written (`user@host:port` minus brackets decoding), used to
    /// match configured host names that contain reserved characters.
    pub authority: String,
    pub user: Option<String>,
    /// Decoded host; an IPv6 literal without its brackets.
    pub host: String,
    pub ipv6: bool,
    pub port: Option<u16>,
    pub path: RemotePath,
}

/// `ssh://` with no host lists hosts; otherwise a concrete URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    Index,
    Url(SshUrl),
}

/// Whether `input` uses the `ssh://` scheme (case-insensitive).
pub fn is_ssh_url(input: &str) -> bool {
    input
        .get(..6)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("ssh://"))
}

const FORM: &str = "use ssh://[user@]host[:1-65535]/<absolute-path>";

/// Parse an `ssh://` URL. Callers check [`is_ssh_url`] first.
pub fn parse(input: &str) -> Result<Parsed, String> {
    if !is_ssh_url(input) {
        return Err(format!("not an ssh:// URL: {input:?}"));
    }
    let rest = &input[6..];
    if rest.chars().any(|c| c.is_control()) {
        return Err(
            "ssh:// URLs cannot contain control characters; percent-encode them".to_owned(),
        );
    }
    if rest.contains('?') {
        return Err(format!(
            "ssh:// does not support URL query strings; percent-encode a literal '?' as %3F in the path: {input}"
        ));
    }
    if rest.contains('#') {
        return Err(format!(
            "ssh:// does not support URL fragments; percent-encode a literal '#' as %23 in the path: {input}"
        ));
    }
    let (auth_raw, path_raw) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if auth_raw.is_empty() {
        if path_raw.is_empty() || path_raw == "/" {
            return Ok(Parsed::Index);
        }
        return Err(format!(
            "ssh:// requires a host before the path: ssh://<host>{path_raw} (host-less ssh://{path_raw} is not valid)"
        ));
    }
    let (user_raw, hostport) = match auth_raw.rfind('@') {
        Some(i) => (Some(&auth_raw[..i]), &auth_raw[i + 1..]),
        None => (None, auth_raw),
    };
    if let Some(user) = user_raw {
        if user.is_empty() {
            return Err(format!(
                "ssh://: empty username in {input:?}; drop the leading '@' or provide a username before it"
            ));
        }
        if user.contains(':') {
            return Err(
                "ssh://: password authentication is not supported; ssh:// uses key/agent auth, drop the ':<password>' from the URL"
                    .to_owned(),
            );
        }
    }
    let (host_raw, port_raw, ipv6) = if let Some(inner) = hostport.strip_prefix('[') {
        let Some(close) = inner.find(']') else {
            return Err(format!(
                "ssh://: unterminated IPv6 literal in {input:?}; {FORM}"
            ));
        };
        let literal = &inner[..close];
        if literal.is_empty()
            || !literal
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
        {
            return Err(format!(
                "ssh://: invalid IPv6 literal [{literal}] in {input:?}"
            ));
        }
        let after = &inner[close + 1..];
        let port = match after {
            "" => None,
            _ => match after.strip_prefix(':') {
                Some(port) => Some(port),
                None => {
                    return Err(format!(
                        "ssh://: unexpected text after IPv6 literal in {input:?}; {FORM}"
                    ));
                }
            },
        };
        (literal, port, true)
    } else {
        match hostport.rsplit_once(':') {
            Some((host, _)) if host.contains(':') => {
                return Err(format!(
                    "ssh://: IPv6 addresses must be bracketed, e.g. ssh://[::1]/path ({input:?})"
                ));
            }
            Some((host, port)) => (host, Some(port), false),
            None => (hostport, None, false),
        }
    };
    let port = match port_raw {
        None => None,
        Some("") => {
            return Err(format!(
                "ssh://: empty port in {input:?}; use ssh://host:<1-65535>/<path> or drop the colon"
            ));
        }
        Some(text) => match text.parse::<u16>() {
            Ok(0) => {
                return Err(
                    "ssh://: port 0 is not a valid SSH port; use ssh://host:<1-65535>/<path> or omit the port"
                        .to_owned(),
                );
            }
            Ok(port) if text.bytes().all(|b| b.is_ascii_digit()) => Some(port),
            _ => return Err(format!("ssh://: invalid host or port in {input:?}; {FORM}")),
        },
    };
    let host = percent_decode(host_raw).map_err(|e| format!("ssh://: {e} in host of {input:?}"))?;
    if host.is_empty() {
        return Err(format!(
            "ssh:// requires a host: ssh://<host>/<absolute-path> ({input:?})"
        ));
    }
    let user = user_raw
        .map(|user| {
            percent_decode(user).map_err(|e| format!("ssh://: {e} in username of {input:?}"))
        })
        .transpose()?;
    check_destination_part("host", &host)?;
    if let Some(user) = &user {
        check_destination_part("username", user)?;
        if user.contains('@') || user.contains(':') {
            return Err(format!("ssh://: invalid username {user:?}"));
        }
    }
    let authority =
        percent_decode(auth_raw).map_err(|e| format!("ssh://: {e} in authority of {input:?}"))?;
    if path_raw.is_empty() {
        return Err(
            "ssh:// requires an absolute path, e.g. ssh://host/etc/hosts or ssh://host/ for the root directory"
                .to_owned(),
        );
    }
    let decoded =
        percent_decode(path_raw).map_err(|e| format!("ssh://: {e} in path of {input:?}"))?;
    if decoded.contains('\0') {
        return Err("ssh://: path cannot contain NUL".to_owned());
    }
    let path = if decoded == "/~" {
        RemotePath::Home(String::new())
    } else if let Some(rest) = decoded.strip_prefix("/~/") {
        RemotePath::Home(rest.to_owned())
    } else {
        RemotePath::Absolute(decoded)
    };
    Ok(Parsed::Url(SshUrl {
        href: input.to_owned(),
        authority,
        user,
        host,
        ipv6,
        port,
        path,
    }))
}

/// A destination part that reaches an `ssh` argv. A leading `-` would be read as an option
/// (`-oProxyCommand=...` is local command execution), so it is refused everywhere.
pub fn check_destination_part(what: &str, value: &str) -> Result<(), String> {
    if value.starts_with('-') {
        return Err(format!(
            "invalid SSH {what} {value:?}: an SSH destination must not begin with '-' (argument-injection guard)"
        ));
    }
    if value
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '/')
    {
        return Err(format!(
            "invalid SSH {what} {value:?}: whitespace, control characters and '/' are not allowed"
        ));
    }
    Ok(())
}

/// Strict percent-decoding: a malformed escape or invalid UTF-8 is an error, never passed through.
pub fn percent_decode(input: &str) -> Result<String, String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3).ok_or("truncated percent-escape")?;
            let text = std::str::from_utf8(hex).map_err(|_| "invalid percent-escape")?;
            let value = u8::from_str_radix(text, 16)
                .map_err(|_| format!("invalid percent-escape %{text}"))?;
            out.push(value);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| "percent-escapes decode to invalid UTF-8".to_owned())
}

/// Single-quote `value` for a POSIX shell.
pub fn sh_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_owned();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests;
