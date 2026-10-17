//! HTML-to-markdown reader chain. Each reader is tried in order; the first output that passes the
//! quality gate wins. External readers get the HTML on stdin and never the URL, so they cannot make
//! requests the SSRF guard has not checked.

use super::content::{self, Kind};
use super::fetch::Client;
use super::html;
use crate::config::types::WebConfig;
use crate::tools::exec::shell::ProcessGroup;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const DEFAULT_CHAIN: [&str; 3] = ["native", "trafilatura", "lynx"];
const READER_TIMEOUT: Duration = Duration::from_secs(10);
/// How often a running external reader re-checks the interrupt flag.
const INTERRUPT_POLL: Duration = Duration::from_millis(50);
const JINA_ENDPOINT: &str = "https://r.jina.ai/";
const JINA_MARKER: &str = "Markdown Content:";
/// Environment variables passed through to external readers; everything else is cleared.
const READER_ENV: [&str; 5] = ["PATH", "HOME", "LANG", "LC_ALL", "TMPDIR"];
const JS_GATED_PHRASES: [&str; 5] = [
    "enable javascript",
    "javascript required",
    "turn on javascript",
    "please enable javascript",
    "browser not supported",
];

/// The readers to try, in order. An empty `web.reader_chain` uses the default chain. `jina` is
/// dropped unless `web.jina_reader` is set. Unknown names are an error.
pub fn chain(config: &WebConfig) -> Result<Vec<&'static str>, String> {
    let names: Vec<String> = if config.reader_chain.is_empty() {
        DEFAULT_CHAIN
            .iter()
            .map(|name| (*name).to_owned())
            .collect()
    } else {
        config.reader_chain.clone()
    };
    let mut readers = Vec::new();
    for name in names {
        match name.trim().to_ascii_lowercase().as_str() {
            "native" => readers.push("native"),
            "trafilatura" => readers.push("trafilatura"),
            "lynx" => readers.push("lynx"),
            "jina" => {
                if config.jina_reader {
                    readers.push("jina");
                }
            }
            other => {
                return Err(format!(
                    "unknown web reader `{other}` in web.reader_chain (known: native, trafilatura, lynx, jina)"
                ));
            }
        }
    }
    Ok(readers)
}

/// Whether a reader's output is good enough to use.
pub fn passes_gate(text: &str) -> bool {
    text.chars().filter(|c| !c.is_whitespace()).count() > 100 && !is_low_quality(text)
}

/// Pages that are just a JavaScript gate, or a wall of short lines (navigation, not prose).
pub fn is_low_quality(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if text.len() < 1024 && JS_GATED_PHRASES.iter().any(|phrase| lower.contains(phrase)) {
        return true;
    }
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let short = lines
        .iter()
        .filter(|line| line.chars().count() < 40)
        .count();
    lines.len() > 10 && short * 10 > lines.len() * 7
}

/// Convert `page` (HTML fetched from `url`) with the reader chain. `Ok(None)` means no reader passed
/// the gate, and the caller should fall back to the built-in conversion.
pub fn convert(
    client: &Client,
    config: &WebConfig,
    workspace: &Path,
    url: &str,
    page: &str,
) -> Result<Option<String>, String> {
    for reader in chain(config)? {
        let produced = match reader {
            "native" => Ok(html::to_text(page, url)),
            "trafilatura" => run_external(
                client,
                workspace,
                "trafilatura",
                &["--output-format", "markdown"],
                page,
            ),
            "lynx" => run_external(
                client,
                workspace,
                "lynx",
                &["-stdin", "-dump", "-nolist", "-width", "250"],
                page,
            ),
            _ => jina(client, url),
        };
        if client.is_interrupted() {
            return Err("web request cancelled".into());
        }
        if let Ok(text) = produced {
            let text = text.trim().to_owned();
            if passes_gate(&text) {
                return Ok(Some(text));
            }
        }
    }
    Ok(None)
}

/// The Jina reader, which fetches the page itself. Only ever sent to r.jina.ai; the key is optional
/// and goes only there.
fn jina(client: &Client, url: &str) -> Result<String, String> {
    let endpoint = format!("{JINA_ENDPOINT}{url}");
    let key = std::env::var("JINA_API_KEY")
        .ok()
        .filter(|key| !key.trim().is_empty());
    let auth = key.map(|key| format!("Bearer {}", key.trim()));
    let mut headers: Vec<(&str, &str)> = vec![("X-No-Cache", "true")];
    if let Some(auth) = auth.as_deref() {
        headers.push(("Authorization", auth));
    }
    let response = client.send(&endpoint, "text/markdown", &headers, None)?;
    if !(200..300).contains(&response.status) {
        return Err(format!("HTTP {} from the jina reader", response.status));
    }
    let body = content::decode(&response.body, response.charset.as_deref(), Kind::Text);
    Ok(match body.find(JINA_MARKER) {
        Some(index) => body[index + JINA_MARKER.len()..].trim().to_owned(),
        None => body,
    })
}

/// Environment for external readers: the minimal allow-list minus anything secret-looking.
fn reader_env() -> Vec<(String, String)> {
    std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(name, value)| {
            READER_ENV.contains(&name.as_str())
                && !crate::tools::exec::sandbox::is_secret_env(name, value)
        })
        .collect()
}

/// Run an external reader with `input` on stdin and return its stdout. No shell; cleared
/// environment; killed after [`READER_TIMEOUT`] or when `client` is interrupted.
fn run_external(
    client: &Client,
    workspace: &Path,
    program: &str,
    args: &[&str],
    input: &str,
) -> Result<String, String> {
    let executable =
        crate::tools::exec::kernel::resolve_operator_executable(workspace, program, "web reader")?;
    let mut command = Command::new(&executable);
    command
        .args(args)
        .env_clear()
        .envs(reader_env())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let child = crate::platform::process::spawn_group(&mut command)
        .map_err(|error| format!("could not start {program}: {error}"))?;
    // Owned from spawn onward: every exit path below terminates and reaps the whole group.
    let mut group = ProcessGroup::new(child);
    let mut stdin = group
        .child
        .stdin
        .take()
        .ok_or_else(|| format!("{program} has no stdin"))?;
    let mut stdout = group
        .child
        .stdout
        .take()
        .ok_or_else(|| format!("{program} has no stdout"))?;
    let payload = input.as_bytes().to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&payload);
    });
    let reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = stdout.read_to_end(&mut output);
        output
    });
    let deadline = Instant::now() + READER_TIMEOUT;
    loop {
        match group.exited() {
            Ok(true) => break,
            Ok(false) => {
                if client.is_interrupted() {
                    return Err(format!("{program} cancelled"));
                }
                if Instant::now() >= deadline {
                    return Err(format!("{program} timed out"));
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                let slice = remaining.min(INTERRUPT_POLL);
                if let Err(error) = crate::platform::process::wait_exited(&group.child, slice) {
                    return Err(format!("could not wait for {program}: {error}"));
                }
            }
            Err(error) => return Err(format!("could not wait for {program}: {error}")),
        }
    }
    // The leader may have exited while group members still hold the pipes: kill them before
    // joining the pipe threads, which would otherwise wait on those members forever.
    group.terminate();
    let status = group
        .child
        .wait()
        .map_err(|error| format!("could not wait for {program}: {error}"))?;
    let _ = writer.join();
    let output = reader
        .join()
        .map_err(|_| format!("{program} output reader panicked"))?;
    if !status.success() {
        return Err(format!("{program} exited with {status}"));
    }
    Ok(String::from_utf8_lossy(&output).into_owned())
}

#[cfg(test)]
mod tests;
