//! `bash` with `trace: true`: run the command under `strace -f` (Linux) and append a bounded
//! summary of what it actually did: programs executed, exit statuses and fatal signals, paths it
//! looked for but did not find, permission failures, files written and read, and network
//! connects/binds. The raw trace stays in the workspace scratch directory for `grep`.
//!
//! Library, locale and interpreter import probes are filtered from the missing-path list; their
//! count is reported so nothing disappears silently.
use std::collections::{BTreeMap, BTreeSet};
use std::io::BufRead;
use std::path::PathBuf;

const LISTED: usize = 25;
const COMMAND_CHARS: usize = 200;

pub(super) struct Trace {
    pub(super) command: String,
    path: PathBuf,
    display: String,
}

/// Validate a traced request and build the wrapped command. `None` when tracing was not asked.
pub(super) fn prepare(
    ws: &crate::tools::Workspace,
    args: &serde_json::Value,
    command: &str,
) -> Result<Option<Trace>, String> {
    if !crate::tools::opt_bool(args, "trace")? {
        return Ok(None);
    }
    for conflicting in ["background", "pty", "session"] {
        if args
            .get(conflicting)
            .is_some_and(|value| !value.is_null() && value != &serde_json::json!(false))
        {
            return Err(format!(
                "trace runs one foreground command and cannot be combined with {conflicting}"
            ));
        }
    }
    if !cfg!(target_os = "linux") {
        return Err("trace needs Linux with strace; nothing was run. On this host use read sys://procs/PID for a running process or the program's own verbose flags".into());
    }
    let super::Runner::Bash(bash) = super::runner_for(ws)? else {
        return Err("trace needs the system bash backend; nothing was run".into());
    };
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let strace = crate::tools::fs::system::find_on_path("strace", &path_var).ok_or(
        "trace needs strace, which is not installed; nothing was run. Install it (for example apt-get install -y strace) or rerun without trace",
    )?;
    let dir = ws.root.join(dial_core::workspace_copy::SCRATCH_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating trace directory: {e}"))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let name = format!("trace-{stamp}-{}.txt", std::process::id());
    let path = dir.join(&name);
    let wrapped = format!(
        "exec {} -f -q -s 256 -e trace=%file,connect,bind -o {} {} -c {}",
        quote(&strace.to_string_lossy()),
        quote(&path.to_string_lossy()),
        quote(&bash.to_string_lossy()),
        quote(command)
    );
    Ok(Some(Trace {
        command: wrapped,
        path,
        display: format!("{}/{name}", dial_core::workspace_copy::SCRATCH_DIR),
    }))
}

fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

impl Trace {
    /// Append the summary to a finished command's output. A missing or empty trace (strace could
    /// not attach, or the command moved to the background first) is reported, not hidden.
    pub(super) fn finish(self, output: &mut crate::tools::ToolOutput) {
        let summary = match std::fs::File::open(&self.path) {
            Ok(file) => {
                let mut summary = Summary::default();
                for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
                    summary.line(&line);
                }
                if summary.lines == 0 {
                    format!("[trace: {} is empty; strace may lack ptrace permission here (see the output above)]", self.display)
                } else {
                    summary.render(&self.display)
                }
            }
            Err(_) => "[trace: no trace was written; strace could not start or attach (see the output above)]".into(),
        };
        if !output.text.ends_with('\n') {
            output.text.push('\n');
        }
        output.text.push_str(&summary);
    }
}

#[derive(Debug, Default)]
pub(super) struct Summary {
    lines: usize,
    pending: BTreeMap<String, String>,
    programs: BTreeMap<String, String>,
    executed: Vec<String>,
    failed_exec: BTreeMap<String, (String, String)>,
    exits: Vec<String>,
    missing: BTreeSet<String>,
    probes: usize,
    denied: BTreeSet<String>,
    written: BTreeSet<String>,
    read: BTreeSet<String>,
    network: BTreeSet<String>,
}

/// Library, locale and module-resolution probes: an interpreter or loader searching its own paths.
fn probe_noise(path: &str) -> bool {
    path.starts_with("/proc/")
        || path.starts_with("/sys/")
        || path.starts_with("/dev/")
        || path.starts_with("/usr/lib")
        || path.starts_with("/usr/local/lib")
        || path.starts_with("/usr/share/locale")
        || path.starts_with("/lib")
        || path.starts_with("/etc/ld.so")
        || path.contains("/__pycache__")
        || path.contains("/site-packages/")
        || path.contains("/dist-packages/")
        || path.contains("/node_modules/")
        || path.contains("gconv")
        || path.ends_with(".pyc")
        || path.ends_with(".so")
        || path.contains(".so.")
}

/// The first double-quoted string in `args`, with strace's backslash escapes kept as written.
fn first_string(args: &str) -> Option<&str> {
    let start = args.find('"')? + 1;
    let bytes = args.as_bytes();
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return args.get(start..i),
            _ => i += 1,
        }
    }
    None
}

fn strings(list: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = list;
    while let Some(found) = first_string(rest) {
        out.push(found);
        let consumed = found.as_ptr() as usize - rest.as_ptr() as usize + found.len() + 1;
        rest = rest.get(consumed..).unwrap_or("");
    }
    out
}

fn between<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let from = text.find(start)? + start.len();
    let len = text[from..].find(end)?;
    Some(&text[from..from + len])
}

/// `host:port` or a Unix socket path from a strace sockaddr.
fn endpoint(args: &str) -> Option<String> {
    if let Some(path) = between(args, "sun_path=\"", "\"") {
        return Some(format!("unix:{path}"));
    }
    let port = between(args, "_port=htons(", ")")?;
    let host = between(args, "inet_addr(\"", "\"")
        .or_else(|| between(args, "inet_pton(AF_INET6, \"", "\""))?;
    Some(if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    })
}

fn shorten(text: &str) -> String {
    let text = crate::storage::redact::redact_text(text);
    if text.chars().count() > COMMAND_CHARS {
        format!("{}…", text.chars().take(COMMAND_CHARS).collect::<String>())
    } else {
        text.into_owned()
    }
}

impl Summary {
    pub(super) fn line(&mut self, raw: &str) {
        self.lines += 1;
        let (pid, text) = match raw.split_once(char::is_whitespace) {
            Some((pid, rest)) if pid.chars().all(|c| c.is_ascii_digit()) => {
                (pid.to_owned(), rest.trim_start())
            }
            _ => (String::new(), raw),
        };
        if let Some(status) = text
            .strip_prefix("+++ ")
            .and_then(|t| t.strip_suffix(" +++"))
        {
            let program = self
                .programs
                .get(&pid)
                .cloned()
                .unwrap_or_else(|| "?".into());
            if status != "exited with 0" {
                self.exits.push(format!("pid {pid} ({program}) {status}"));
            }
            return;
        }
        if text.starts_with("--- ") {
            return;
        }
        let joined;
        let text = if let Some(head) = text.strip_suffix(" <unfinished ...>") {
            self.pending.insert(pid, head.to_owned());
            return;
        } else if let Some(rest) = text.strip_prefix("<... ") {
            let Some((_, tail)) = rest.split_once(" resumed>") else {
                return;
            };
            let Some(head) = self.pending.remove(&pid) else {
                return;
            };
            joined = format!("{head}{tail}");
            joined.as_str()
        } else {
            text
        };
        self.call(&pid, text);
    }

    fn call(&mut self, pid: &str, text: &str) {
        let Some(open) = text.find('(') else {
            return;
        };
        let name = &text[..open];
        let Some(eq) = text.rfind(") = ") else {
            return;
        };
        let args = &text[open + 1..eq];
        let result = &text[eq + 4..];
        let errno = result
            .strip_prefix("-1 ")
            .and_then(|rest| rest.split_whitespace().next());
        match name {
            "execve" | "execveat" => {
                let Some(path) = first_string(args) else {
                    return;
                };
                let base = path.rsplit('/').next().unwrap_or(path).to_owned();
                match errno {
                    None => {
                        let argv = args
                            .find('[')
                            .and_then(|start| Some((start, args[start..].find(']')? + start)))
                            .map(|(start, end)| strings(&args[start..=end]).join(" "))
                            .unwrap_or_else(|| path.to_owned());
                        self.programs.insert(pid.to_owned(), base.clone());
                        self.failed_exec.remove(&base);
                        if self.executed.len() < 200 {
                            self.executed.push(shorten(&argv));
                        }
                    }
                    Some(errno) => {
                        self.failed_exec
                            .entry(base)
                            .or_insert_with(|| (path.to_owned(), errno.to_owned()));
                    }
                }
            }
            "connect" | "bind" => {
                if let Some(endpoint) = endpoint(args) {
                    let outcome = match errno {
                        None => "ok",
                        Some("EINPROGRESS") => "in progress",
                        Some(errno) => errno,
                    };
                    let program = self
                        .programs
                        .get(pid)
                        .cloned()
                        .unwrap_or_else(|| "?".into());
                    self.network
                        .insert(format!("{name} {endpoint} {outcome} ({program})"));
                }
            }
            _ => {
                let Some(path) = first_string(args) else {
                    return;
                };
                if path.is_empty() {
                    return;
                }
                match errno {
                    Some("ENOENT") | Some("ENOTDIR") => {
                        if probe_noise(path) || path.ends_with(".py") && name != "openat" {
                            self.probes += 1;
                        } else {
                            self.missing.insert(format!("{path} ({name})"));
                        }
                    }
                    Some(errno @ ("EACCES" | "EPERM" | "EROFS")) => {
                        if !path.starts_with("/proc/") && !path.starts_with("/sys/") {
                            self.denied.insert(format!("{path} ({name} {errno})"));
                        }
                    }
                    Some(_) => {}
                    None => {
                        let writes = matches!(
                            name,
                            "creat"
                                | "mkdir"
                                | "mkdirat"
                                | "unlink"
                                | "unlinkat"
                                | "rename"
                                | "renameat"
                                | "renameat2"
                                | "truncate"
                                | "chmod"
                                | "fchmodat"
                        ) || (name.starts_with("open")
                            && ["O_WRONLY", "O_RDWR", "O_CREAT", "O_TRUNC"]
                                .iter()
                                .any(|flag| args.contains(flag)));
                        if writes {
                            if !path.starts_with("/dev/") {
                                self.written.insert(format!("{path} ({name})"));
                            }
                        } else if name.starts_with("open")
                            && !args.contains("O_DIRECTORY")
                            && !probe_noise(path)
                            && !path.starts_with("/etc/")
                        {
                            self.read.insert(path.to_owned());
                        }
                    }
                }
            }
        }
    }

    pub(super) fn render(mut self, raw: &str) -> String {
        for (path, errno) in std::mem::take(&mut self.failed_exec).into_values() {
            if errno == "ENOENT" && !path.contains('/') {
                continue;
            }
            self.missing.insert(format!("{path} (execve {errno})"));
        }
        let mut out = format!("[trace: strace -f summary; raw trace {raw}]\n");
        let mut section = |title: &str, items: Vec<String>| {
            if items.is_empty() {
                return;
            }
            out.push_str(&format!("{title} ({}):\n", items.len()));
            for item in items.iter().take(LISTED) {
                out.push_str(&format!("  {item}\n"));
            }
            if items.len() > LISTED {
                out.push_str(&format!(
                    "  [{} more in the raw trace]\n",
                    items.len() - LISTED
                ));
            }
        };
        section("executed", std::mem::take(&mut self.executed));
        section("abnormal exits", std::mem::take(&mut self.exits));
        section(
            "not found",
            std::mem::take(&mut self.missing).into_iter().collect(),
        );
        section(
            "permission denied",
            std::mem::take(&mut self.denied).into_iter().collect(),
        );
        section(
            "network",
            std::mem::take(&mut self.network).into_iter().collect(),
        );
        section(
            "written",
            std::mem::take(&mut self.written).into_iter().collect(),
        );
        section("read", std::mem::take(&mut self.read).into_iter().collect());
        if self.probes > 0 {
            out.push_str(&format!(
                "[{} library/module search probes that found nothing are omitted]\n",
                self.probes
            ));
        }
        out.trim_end().into()
    }
}

/// For tests: the trace summary of a complete strace output.
#[cfg(test)]
pub(super) fn summarize(trace: &str) -> String {
    let mut summary = Summary::default();
    for line in trace.lines() {
        summary.line(line);
    }
    summary.render(".tmp/t.txt")
}

#[cfg(test)]
mod tests;
