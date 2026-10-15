//! Out-of-process document parsing. Parsers see hostile input (spreadsheets, notebooks, archives,
//! jq programs over arbitrary JSON), so every job runs in a fresh copy of this executable. The
//! worker reads one job from stdin, writes one framed outcome to stdout, and exits. The parent
//! enforces the wall deadline, cancellation, an output cap and, on Unix, CPU and address-space
//! limits, and kills the whole worker group on any of them. A panic, abort, or runaway allocation
//! in a parser ends only the worker.
//!
//! Forking the agent instead is unsound: the agent is multithreaded, so a lock held by another
//! thread at fork time (allocator, SQLite, std) leaves the forked child blocked forever.

use std::{
    fs,
    io::{self, Read, Seek, Write},
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::platform;
use crate::tools::Workspace;

use super::cache::Opened;

/// Frame markers on stdout. The payload is compact JSON with `<` escaped, so a marker can never
/// occur inside it.
const BEGIN: &[u8] = b"\n<<dial-document-worker:begin>>\n";
const END: &[u8] = b"\n<<dial-document-worker:end>>\n";
/// Set on every worker. The in-binary test entry point runs only when it is present.
const WORKER_ENV: &str = "DIAL_DOCUMENT_WORKER";
/// argv marker for the production worker; `main` dispatches it before CLI parsing.
pub const WORKER_ARG: &str = "__document-worker";
/// Largest job (JSON header plus payload) a worker accepts.
const MAX_JOB: u64 = super::MAX_INPUT + 64 * 1024;
/// Largest outcome frame the parent accepts.
const MAX_FRAME: u64 = 256 * 1024 * 1024;
const POLL: Duration = Duration::from_millis(10);
const EXIT_GRACE: Duration = Duration::from_secs(5);

/// One unit of parser work. The worker re-derives everything from these fields; paths were
/// authorized by the parent before dispatch.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "job", rename_all = "snake_case")]
pub(super) enum Job {
    Sqlite {
        path: PathBuf,
        member: Option<String>,
    },
    Notebook {
        path: PathBuf,
        member: Option<String>,
    },
    /// `root` is the workspace root the parent authorized `path` under.
    Archive {
        root: PathBuf,
        path: PathBuf,
        format: String,
        member: Option<String>,
    },
    Office {
        path: PathBuf,
        format: String,
        member: Option<String>,
    },
    /// EPUB package; `member` is an optional `chapter=N` spine selector.
    Epub {
        path: PathBuf,
        member: Option<String>,
    },
    Json {
        path: PathBuf,
        label: String,
        query: String,
    },
    /// In-memory document; the bytes travel as the payload after the JSON header.
    Convert { format: String },
    #[cfg(test)]
    Probe { action: String },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum Outcome {
    Text { text: String },
    Failed { message: String },
    Panicked,
}

/// Run `job` in a worker process and return its text. `payload` is written to the worker's stdin
/// after the job header. Errors: the parser's own error, a panic or crash, the wall deadline,
/// cancellation, or an oversized output. The worker is always reaped before returning.
pub(super) fn run(
    label: &str,
    job: &Job,
    payload: &[u8],
    deadline: Duration,
    cancel: &AtomicBool,
) -> Result<String, String> {
    if cancel.load(Ordering::Acquire) {
        return Err(format!("{label} cancelled"));
    }
    let mut header = serde_json::to_vec(job).map_err(|e| format!("{label}: encoding job: {e}"))?;
    header.push(b'\n');
    let exe =
        std::env::current_exe().map_err(|e| format!("{label}: locating worker executable: {e}"))?;
    let mut command = Command::new(exe);
    command
        .args(worker_args())
        .env(WORKER_ENV, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    limit(&mut command, deadline);
    let mut child = platform::process::spawn_group(&mut command)
        .map_err(|e| format!("{label}: starting worker: {e}"))?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let (Some(mut stdin), Some(stdout)) = (stdin, stdout) else {
        stop(&mut child);
        return Err(format!("{label}: worker pipes were not created"));
    };
    thread::scope(|scope| {
        // The writer ends on its own: once the worker reads all input, or dies (EPIPE).
        scope.spawn(move || {
            let _ = stdin
                .write_all(&header)
                .and_then(|()| stdin.write_all(payload));
        });
        supervise(label, &mut child, stdout, deadline, cancel)
    })
}

fn supervise(
    label: &str,
    child: &mut Child,
    mut stdout: impl Read + Send + 'static,
    deadline: Duration,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut data = Vec::new();
        let read = stdout
            .by_ref()
            .take(MAX_FRAME + 1)
            .read_to_end(&mut data)
            .map(|_| data);
        let _ = tx.send(read);
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                stop(child);
                return Err(format!("{label}: waiting for worker: {e}"));
            }
        }
        if cancel.load(Ordering::Acquire) {
            stop(child);
            return Err(format!("{label} cancelled; isolated worker killed"));
        }
        if started.elapsed() >= deadline {
            stop(child);
            return Err(format!(
                "{label} exceeded {deadline:?} and was stopped; simplify or narrow the request"
            ));
        }
        thread::sleep(POLL);
    };
    let data = match rx.recv_timeout(EXIT_GRACE) {
        Ok(Ok(data)) => data,
        Ok(Err(e)) => return Err(format!("{label}: reading worker output: {e}")),
        Err(_) => return Err(format!("{label}: worker output did not close")),
    };
    if data.len() as u64 > MAX_FRAME {
        return Err(format!("{label}: worker output exceeds {MAX_FRAME} bytes"));
    }
    match frame(&data) {
        Some(payload) => decode(label, payload),
        None => Err(format!(
            "{label} worker ended without a result ({status}); the input may be malformed"
        )),
    }
}

/// Kill the worker's whole process group (Unix) or job object (Windows), then reap it.
fn stop(child: &mut Child) {
    let _ = platform::process::kill_group(child.id());
    let _ = child.wait();
}

fn frame(data: &[u8]) -> Option<&[u8]> {
    let start = find(data, BEGIN)? + BEGIN.len();
    let len = find(&data[start..], END)?;
    Some(&data[start..start + len])
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn decode(label: &str, payload: &[u8]) -> Result<String, String> {
    match serde_json::from_slice::<Outcome>(payload) {
        Ok(Outcome::Text { text }) => Ok(text),
        Ok(Outcome::Failed { message }) => Err(message),
        Ok(Outcome::Panicked) => Err(format!(
            "{label} failed internally on this input (parser panic); the input may be malformed"
        )),
        Err(e) => Err(format!("{label}: invalid worker outcome: {e}")),
    }
}

#[cfg(test)]
fn worker_args() -> Vec<&'static str> {
    // Under `cargo test` the current executable is the test binary: run only the worker entry.
    vec![
        "tools::documents::isolate::tests::worker_entry",
        "--exact",
        "--nocapture",
        "--test-threads=1",
    ]
}

#[cfg(not(test))]
fn worker_args() -> Vec<&'static str> {
    vec![WORKER_ARG]
}

/// Child-side CPU and address-space limits, applied between fork and exec.
#[cfg(unix)]
fn limit(command: &mut Command, deadline: Duration) {
    use std::os::unix::process::CommandExt;
    let cpu = deadline.as_secs().saturating_add(1) as libc::rlim_t;
    let memory = address_space_limit().map(|bytes| bytes as libc::rlim_t);
    // SAFETY: the closure only calls setrlimit, which is async-signal-safe, and allocates nothing.
    unsafe {
        command.pre_exec(move || {
            let cpu_limit = libc::rlimit {
                rlim_cur: cpu,
                rlim_max: cpu.saturating_add(1),
            };
            if libc::setrlimit(libc::RLIMIT_CPU, &cpu_limit) != 0 {
                return Err(io::Error::last_os_error());
            }
            if let Some(bytes) = memory {
                let memory_limit = libc::rlimit {
                    rlim_cur: bytes,
                    rlim_max: bytes,
                };
                if libc::setrlimit(libc::RLIMIT_AS, &memory_limit) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn limit(_command: &mut Command, _deadline: Duration) {
    // Windows bounds the worker by its job object and the parent's deadline.
}

/// Address-space cap for the worker, in bytes. RLIMIT_AS counts every mapping the child inherits,
/// so a fixed cap would starve a parent that already maps more than that. Linux only: macOS does
/// not enforce RLIMIT_AS, and the wall-clock deadline still bounds the worker there.
#[cfg(target_os = "linux")]
fn address_space_limit() -> Option<u64> {
    const PARSER_ADDRESS_SPACE: u64 = 4 << 30;
    let statm = fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().next()?.parse().ok()?;
    let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
        .ok()
        .filter(|p| *p > 0)?;
    Some(
        pages
            .saturating_mul(page)
            .saturating_add(PARSER_ADDRESS_SPACE),
    )
}

#[cfg(all(unix, not(target_os = "linux")))]
fn address_space_limit() -> Option<u64> {
    None
}

/// Worker entry point. Reads one job from stdin, writes one framed outcome to stdout, and exits.
/// Never returns.
pub fn worker_main() -> ! {
    let outcome = match read_job() {
        Ok((job, payload)) => {
            match panic::catch_unwind(AssertUnwindSafe(|| execute(&job, &payload))) {
                Ok(Ok(text)) => Outcome::Text { text },
                Ok(Err(message)) => Outcome::Failed { message },
                Err(_) => Outcome::Panicked,
            }
        }
        Err(message) => Outcome::Failed { message },
    };
    let body = serde_json::to_vec(&outcome).unwrap_or_default();
    let mut escaped = Vec::with_capacity(body.len());
    for byte in body {
        if byte == b'<' {
            escaped.extend_from_slice(br"\u003c");
        } else {
            escaped.push(byte);
        }
    }
    let mut out = io::stdout().lock();
    let _ = out
        .write_all(BEGIN)
        .and_then(|()| out.write_all(&escaped))
        .and_then(|()| out.write_all(END))
        .and_then(|()| out.flush());
    std::process::exit(0)
}

fn read_job() -> Result<(Job, Vec<u8>), String> {
    let mut input = Vec::new();
    io::stdin()
        .take(MAX_JOB + 1)
        .read_to_end(&mut input)
        .map_err(|e| format!("reading worker input: {e}"))?;
    if input.len() as u64 > MAX_JOB {
        return Err("worker input exceeds the document size limit".into());
    }
    let split = input
        .iter()
        .position(|&b| b == b'\n')
        .ok_or_else(|| "worker input has no job header".to_string())?;
    let job: Job =
        serde_json::from_slice(&input[..split]).map_err(|e| format!("invalid worker job: {e}"))?;
    let payload = input.split_off(split + 1);
    Ok((job, payload))
}

fn execute(job: &Job, payload: &[u8]) -> Result<String, String> {
    match job {
        Job::Sqlite { path, member } => super::sqlite::read(path, member.as_deref()),
        Job::Notebook { path, member } => super::notebook(open(path)?, member.as_deref()),
        Job::Archive {
            root,
            path,
            format,
            member,
        } => {
            let ws = Workspace::new(root).map_err(|e| format!("{e:#}"))?;
            let opened = Opened::open(&ws, path)?;
            super::archives::read(&opened, &ws, format, member.as_deref())
        }
        Job::Office {
            path,
            format,
            member,
        } => super::office::read(open(path)?, format, member.as_deref()),
        Job::Epub { path, member } => super::epub::read(open(path)?, member.as_deref()),
        Job::Json { path, label, query } => super::json::read(path, label, query),
        Job::Convert { format } => {
            let mut file =
                tempfile::tempfile().map_err(|e| format!("creating parser input: {e}"))?;
            file.write_all(payload)
                .map_err(|e| format!("writing parser input: {e}"))?;
            file.rewind()
                .map_err(|e| format!("rewinding parser input: {e}"))?;
            super::convert_stream(file, format)
        }
        #[cfg(test)]
        Job::Probe { action } => probe(action),
    }
}

fn open(path: &Path) -> Result<fs::File, String> {
    fs::File::open(path).map_err(|e| format!("opening document: {e}"))
}

#[cfg(test)]
fn probe(action: &str) -> Result<String, String> {
    match action {
        "text" => Ok("alpha\nbeta <tag>".into()),
        "failed" => Err("bad input".into()),
        "panic" => panic!("boom"),
        "abort" => std::process::abort(),
        "hang" => loop {
            thread::sleep(Duration::from_secs(1));
        },
        other => Err(format!("unknown probe {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe_job(action: &str) -> Job {
        Job::Probe {
            action: action.to_string(),
        }
    }

    /// Worker entry for `cargo test`: runs only when the parent spawns this binary as a worker.
    #[test]
    fn worker_entry() {
        if std::env::var_os(WORKER_ENV).is_some() {
            worker_main();
        }
    }

    #[test]
    fn worker_returns_text_errors_and_contains_panics_and_aborts() {
        let cancel = AtomicBool::new(false);
        let deadline = Duration::from_secs(60);
        assert_eq!(
            run("test", &probe_job("text"), &[], deadline, &cancel),
            Ok("alpha\nbeta <tag>".to_string())
        );
        assert_eq!(
            run("test", &probe_job("failed"), &[], deadline, &cancel),
            Err("bad input".to_string())
        );
        let panicked = run("test", &probe_job("panic"), &[], deadline, &cancel).unwrap_err();
        assert!(panicked.contains("failed internally"), "{panicked}");
        let aborted = run("test", &probe_job("abort"), &[], deadline, &cancel).unwrap_err();
        assert!(
            aborted.contains("without a result") || aborted.contains("crashed"),
            "{aborted}"
        );
    }

    #[test]
    fn runaway_worker_is_killed_at_the_deadline() {
        let cancel = AtomicBool::new(false);
        let started = Instant::now();
        let err = run(
            "test",
            &probe_job("hang"),
            &[],
            Duration::from_millis(500),
            &cancel,
        )
        .unwrap_err();
        assert!(err.contains("exceeded"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    #[test]
    fn cancellation_stops_a_running_worker() {
        let cancel = AtomicBool::new(false);
        let err = thread::scope(|scope| {
            scope.spawn(|| {
                thread::sleep(Duration::from_millis(300));
                cancel.store(true, Ordering::Release);
            });
            run(
                "test",
                &probe_job("hang"),
                &[],
                Duration::from_secs(60),
                &cancel,
            )
            .unwrap_err()
        });
        assert!(err.contains("cancelled"), "{err}");
    }
}
