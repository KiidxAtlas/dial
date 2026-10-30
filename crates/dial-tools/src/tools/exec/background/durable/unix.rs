use super::super::{Background, MAX_COMMAND, Table, lock};
use crate::platform::{fs as private_fs, procinfo::ProcId};
use crate::tools::exec::sandbox::Policy;
use crate::tools::{ToolOutput, Workspace, opt_bool, opt_str, opt_u64, present, req_str};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::{
    fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    net::{UnixListener, UnixStream},
    process::CommandExt,
};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const MAX_WIRE: u64 = 512 * 1024;
const MAX_LIFETIME: u64 = 86400;
const MAX_JOBS: usize = 8;

#[derive(Serialize, Deserialize)]
struct Launch {
    directory: PathBuf,
    root: PathBuf,
    roots: Vec<PathBuf>,
    strict: bool,
    deny: Vec<PathBuf>,
    writes: Vec<PathBuf>,
    network: bool,
    unsandboxed: bool,
    backend: crate::config::types::ShellBackend,
    cargo_session: Option<String>,
    artifacts: Option<PathBuf>,
    args: Value,
    lifetime: u64,
}

#[derive(Serialize, Deserialize)]
struct Record {
    owner: ProcId,
    root: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct Reply {
    text: String,
    is_error: bool,
    data: Option<Value>,
    execution: crate::runtime::execution::Execution,
}
impl From<ToolOutput> for Reply {
    fn from(output: ToolOutput) -> Self {
        Self {
            text: output.text,
            is_error: output.is_error,
            data: output.data,
            execution: output.execution,
        }
    }
}
impl From<Reply> for ToolOutput {
    fn from(reply: Reply) -> Self {
        Self {
            text: reply.text,
            is_error: reply.is_error,
            data: reply.data,
            execution: reply.execution,
        }
    }
}

fn private_dir(path: &Path) -> Result<()> {
    let directory = private_fs::open_dir_nofollow(path)?;
    ensure!(
        private_fs::owned_by_current_user(&directory)? && private_fs::is_owner_only(&directory)?,
        "control directory must be owned by this user and mode 0700"
    );
    Ok(())
}

fn create_dir(path: &Path) -> Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    private_dir(path)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let file = private_fs::open_nofollow_nonblocking(OpenOptions::new().read(true), path)?;
    ensure!(
        file.metadata()?.is_file()
            && private_fs::owned_by_current_user(&file)?
            && private_fs::is_owner_only(&file)?,
        "unsafe control record"
    );
    Ok(serde_json::from_reader(file.take(MAX_WIRE))?)
}

fn publish(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().context("record parent")?)?;
    temp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    serde_json::to_writer(&mut temp, value)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn registry(ws: &Workspace) -> Result<PathBuf> {
    let home = crate::config::paths::user_dir()?;
    // Do not create or relax the user's existing state directory permissions.
    if !home.exists() {
        fs::create_dir_all(&home)?;
        private_fs::set_owner_only(&home)?;
    }
    private_dir(&home)?;
    let jobs = home.join("background-jobs");
    create_dir(&jobs)?;
    let hash = format!(
        "{:x}",
        Sha256::digest(ws.root.as_os_str().as_encoded_bytes())
    );
    let project = jobs.join(&hash[..24]);
    create_dir(&project)?;
    Ok(project)
}

fn name(args: &Value) -> Result<&str> {
    let name = req_str(args, "name").map_err(anyhow::Error::msg)?;
    ensure!(
        !name.is_empty()
            && name.len() <= 32
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
            && name != "."
            && name != "..",
        "name must be 1..32 letters, digits, - _ or . (not . or ..)"
    );
    Ok(name)
}

fn job_dirs(registry: &Path) -> Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    for entry in fs::read_dir(registry)? {
        let entry = entry?;
        if entry.file_name() == ".lock" {
            continue;
        }
        ensure!(
            dirs.len() < MAX_JOBS,
            "durable job registry exceeds its limit"
        );
        private_dir(&entry.path())?;
        dirs.push(entry.path());
    }
    dirs.sort();
    Ok(dirs)
}

struct AdmissionLock(std::fs::File);

impl Drop for AdmissionLock {
    fn drop(&mut self) {
        // Explicit unlock releases inherited flock descriptions too: another test/controller
        // can fork concurrently before close-on-exec takes effect in its child.
        let _ = FileExt::unlock(&self.0);
    }
}

fn confirmed_dead(owner: ProcId) -> bool {
    if let Some(current) = ProcId::of(owner.pid) {
        return current != owner;
    }
    // Unlike an unreadable start identity, ESRCH confirms there is no process at this PID.
    // SAFETY: signal zero only probes existence; it never delivers a signal.
    (unsafe { libc::kill(owner.pid, 0) }) < 0
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

fn forget(directory: &Path) -> Result<()> {
    for file in ["owner.json", "ready.json", "finished.json", "control.sock"] {
        let path = directory.join(file);
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    fs::remove_dir(directory)?;
    Ok(())
}

fn request(directory: &Path, ws: &Workspace, args: &Value) -> Result<ToolOutput> {
    private_dir(directory)?;
    let record: Record = read_json(&directory.join("owner.json"))?;
    ensure!(record.root == ws.root, "job belongs to another workspace");
    if !record.owner.alive() {
        let finished = if directory.join("finished.json").exists() {
            Some(ToolOutput::from(read_json::<Reply>(
                &directory.join("finished.json"),
            )?))
        } else {
            None
        };
        if args["action"] == "kill" {
            ensure!(
                confirmed_dead(record.owner),
                "owner exit not confirmed; retry explicit forget after it exits"
            );
            forget(directory)?;
            return Ok(if finished.is_some() {
                ToolOutput::ok("forgot finished durable job".into())
            } else {
                ToolOutput::err("forgot interrupted durable job; final observation unavailable; command was NOT restarted; effects may be partial")
                    .with_effects(crate::runtime::execution::Effects::Unknown)
            });
        }
        if let Some(output) = finished {
            return Ok(output);
        }
        bail!(
            "owner interrupted or identity unverifiable; command was NOT restarted; effects may be partial"
        );
    }
    let exchange = (|| -> Result<ToolOutput> {
        let mut stream = UnixStream::connect(directory.join("control.sock"))?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;
        // Recheck after connecting, before sending control authority to the endpoint.
        ensure!(
            record.owner.alive(),
            "owner identity changed while connecting"
        );
        serde_json::to_writer(&mut stream, args)?;
        stream.shutdown(std::net::Shutdown::Write)?;
        Ok(serde_json::from_reader::<_, Reply>(stream.take(MAX_WIRE))?.into())
    })();
    // The job can finish between identity validation and socket reply. Observe the atomic final
    // record in that race; never reconnect by spawning or replaying the command.
    exchange.or_else(|error| {
        if directory.join("finished.json").exists() {
            Ok(read_json::<Reply>(&directory.join("finished.json"))?.into())
        } else {
            Err(error)
        }
    })
}

pub(super) fn dispatch(
    ws: &Workspace,
    args: &Value,
    cancel: &AtomicBool,
    interrupt: &AtomicBool,
) -> Result<ToolOutput> {
    ensure!(
        !cancel.load(Ordering::Acquire) && !interrupt.load(Ordering::Acquire),
        "cancelled before durable action"
    );
    let action = req_str(args, "action").map_err(anyhow::Error::msg)?;
    ensure!(
        matches!(action, "start" | "list" | "output" | "wait" | "kill"),
        "durable jobs support only start/list/output/wait/kill; pipe-only, stdin closed"
    );
    let registry = registry(ws)?;
    if action == "start" {
        return start(ws, args, &registry, cancel);
    }
    if action == "list"
        || (action == "kill" && opt_bool(args, "all").map_err(anyhow::Error::msg)?)
    {
        let mut lines = Vec::new();
        let mut failed = false;
        for directory in job_dirs(&registry)? {
            let job = directory.file_name().context("job name")?.to_string_lossy();
            let request_args =
                json!({"action": if action == "kill" { "kill" } else { "list" }, "name": "job"});
            match request(&directory, ws, &request_args) {
                Ok(output) => {
                    failed |= output.is_error;
                    lines.push(format!("{job}: {}", output.text));
                }
                Err(error) => {
                    failed = true;
                    lines.push(format!("{job}: {error:#}"));
                }
            }
        }
        let text = if lines.is_empty() {
            "no durable background jobs".into()
        } else {
            lines.join("\n")
        };
        return Ok(if failed {
            ToolOutput::err(text)
        } else {
            ToolOutput::ok(text)
        });
    }
    let directory = registry.join(name(args)?);
    let mut forwarded = args.clone();
    forwarded["name"] = json!("job");
    if action != "wait" {
        return request(&directory, ws, &forwarded);
    }
    ensure!(
        args.get("ready").is_none(),
        "durable wait does not support ready; use until for log matching"
    );
    let seconds = opt_u64(args, "timeout")
        .map_err(anyhow::Error::msg)?
        .unwrap_or(5);
    ensure!(seconds <= 300, "wait timeout must be at most 300 seconds");
    let pattern = opt_str(args, "until")
        .map_err(anyhow::Error::msg)?
        .map(super::super::compile)
        .transpose()
        .map_err(anyhow::Error::msg)?;
    let end = Instant::now() + Duration::from_secs(if seconds == 0 { 300 } else { seconds });
    forwarded["action"] = json!("output");
    loop {
        ensure!(
            !cancel.load(Ordering::Acquire) && !interrupt.load(Ordering::Acquire),
            "wait cancelled; durable job remains owned"
        );
        let output = request(&directory, ws, &forwarded)?;
        let record: Record = read_json(&directory.join("owner.json"))?;
        if output.is_error
            || !record.owner.alive()
            || pattern
                .as_ref()
                .is_some_and(|regex| regex.is_match(&output.text))
        {
            return Ok(output);
        }
        if Instant::now() >= end {
            return Ok(ToolOutput::ok(format!(
                "wait timed out; job remains owned\n{}",
                output.text
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn start(ws: &Workspace, args: &Value, registry: &Path, cancel: &AtomicBool) -> Result<ToolOutput> {
    start_with_owner(ws, args, registry, cancel, &std::env::current_exe()?)
}

fn start_with_owner(
    ws: &Workspace,
    args: &Value,
    registry: &Path,
    cancel: &AtomicBool,
    executable: &Path,
) -> Result<ToolOutput> {
    ensure!(!cancel.load(Ordering::Acquire), "cancelled before launch");
    let name = name(args)?;
    for field in ["pty", "stdin", "ready", "until", "all", "rows", "cols"] {
        ensure!(
            !present(args, field),
            "durable start does not accept {field}; use a pipe command with closed stdin"
        );
    }
    let lifetime = opt_u64(args, "lifetime")
        .map_err(anyhow::Error::msg)?
        .unwrap_or(3600);
    ensure!(
        (1..=MAX_LIFETIME).contains(&lifetime),
        "lifetime must be 1..86400 seconds"
    );
    let command = req_str(args, "command").map_err(anyhow::Error::msg)?;
    ensure!(
        !command.trim().is_empty() && command.len() <= MAX_COMMAND,
        "command must be nonempty and at most 65536 bytes"
    );
    let cwd = match opt_str(args, "cwd").map_err(anyhow::Error::msg)? {
        Some(path) => ws.resolve(path).map_err(anyhow::Error::msg)?,
        None => ws.root.clone(),
    };
    ensure!(cwd.is_dir(), "cwd is not a directory");
    if let Some(refusal) = crate::tools::exec::git_guard::guard(ws, command, &cwd) {
        return Ok(refusal);
    }
    // Serialize admission across independent controllers. Never prune or relaunch a live job.
    let lock = private_fs::open_nofollow(
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600),
        &registry.join(".lock"),
    )?;
    lock.try_lock_exclusive()
        .context("another controller is admitting a durable job")?;
    let lock = AdmissionLock(lock);
    let dirs = job_dirs(registry)?;
    ensure!(
        dirs.len() < MAX_JOBS,
        "at most 8 retained durable jobs; kill a finished named job to forget its record"
    );
    let directory = registry.join(name);
    ensure!(
        directory
            .join("control.sock")
            .as_os_str()
            .as_encoded_bytes()
            .len()
            < 104,
        "private socket path exceeds Unix limit; use a shorter job name or private DIAL_HOME path"
    );
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&directory)
        .context("job name already retained or cannot be created")?;
    drop(lock);
    let (strict, mut deny, writes, network) = match ws.sandbox_policy() {
        Policy::Strict {
            deny_read,
            extra_write,
            allow_network,
        } => (true, deny_read, extra_write, allow_network),
        Policy::Dev {
            deny_read,
            extra_write,
            allow_network,
        } => (false, deny_read, extra_write, allow_network),
    };
    deny.push(registry.parent().context("registry parent")?.to_owned());
    let mut forwarded = args.clone();
    forwarded["name"] = json!("job");
    forwarded["cwd"] = json!(cwd);
    let launch = Launch {
        directory: directory.clone(),
        root: ws.root.clone(),
        roots: ws.additional_roots.clone(),
        strict,
        deny,
        writes,
        network,
        unsandboxed: ws.unsandboxed,
        backend: ws.shell_backend,
        cargo_session: ws.cargo_session.clone(),
        artifacts: ws.artifacts().map(|store| store.root().to_owned()),
        args: forwarded,
        lifetime,
    };
    let mut spawn = Command::new(executable);
    #[cfg(not(test))]
    spawn.arg(super::OWNER_ARG);
    #[cfg(test)]
    spawn
        .args([
            "--exact",
            "tools::exec::background::durable::unix::tests::owner_fixture",
            "--nocapture",
        ])
        .env("DIAL_BG_TEST_OWNER", "1");
    spawn
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe and touches no Rust state in the pre-exec child.
    unsafe {
        spawn.pre_exec(|| {
            if libc::setsid() < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = match spawn.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = fs::remove_dir(&directory);
            return Err(error.into());
        }
    };
    // Publish identity before delivering the one-use contract. Even an interrupted controller
    // leaves a verifiable owner, rather than an anonymous directory occupying a quota slot.
    let identity = (|| -> Result<()> {
        let owner =
            ProcId::of(child.id() as i32).context("cannot establish owner kernel identity")?;
        publish(
            &directory.join("owner.json"),
            &Record {
                owner,
                root: ws.root.clone(),
            },
        )
    })();
    if let Err(error) = identity {
        // No launch contract was sent, so this child cannot have started the command.
        drop(child.stdin.take());
        let _ = child.kill();
        if child.wait().is_ok() {
            forget(&directory)?;
        }
        return Err(error);
    }
    let launch_result = (|| -> Result<ToolOutput> {
        let end = Instant::now() + Duration::from_secs(10);
        let mut stdin = child.stdin.take().context("owner launch pipe missing")?;
        let payload = serde_json::to_vec(&launch)?;
        ensure!(
            payload.len() as u64 <= MAX_WIRE,
            "owner launch contract exceeds transfer limit"
        );
        crate::platform::io::set_nonblocking(&stdin)?;
        let mut remaining = payload.as_slice();
        while !remaining.is_empty() {
            ensure!(
                Instant::now() < end && !cancel.load(Ordering::Acquire),
                "launch acknowledgement interrupted; command is not retried; inspect durable list"
            );
            match stdin.write(remaining) {
                Ok(0) => bail!("owner launch pipe closed; command is not retried"),
                Ok(written) => remaining = &remaining[written..],
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    crate::platform::io::wait_writable(&stdin, Duration::from_millis(25))?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        }
        drop(stdin);
        loop {
            if directory.join("ready.json").exists() || directory.join("finished.json").exists() {
                return request(&directory, ws, &json!({"action":"list"}));
            }
            if let Some(status) = child.try_wait()? {
                record_launch_failure(
                    &directory,
                    &format!("owner exited before acknowledgement ({status})"),
                )?;
                return request(&directory, ws, &json!({"action":"list"}));
            }
            ensure!(
                Instant::now() < end && !cancel.load(Ordering::Acquire),
                "launch acknowledgement interrupted; command is not retried; inspect durable list"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    })();
    // Reap the direct helper without owning its lifetime; a controller exit does not kill it.
    std::thread::spawn(move || {
        if let Ok(status) = child.wait()
            && !directory.join("ready.json").exists()
            && !directory.join("finished.json").exists()
        {
            let _ = record_launch_failure(
                &directory,
                &format!("owner exited before acknowledgement ({status})"),
            );
        }
    });
    launch_result.map(|mut output| {
        output.text = format!(
            "durable {name} (lifetime {lifetime}s; survives controller exit): {}",
            output.text
        );
        output
    })
}

fn record_launch_failure(directory: &Path, reason: &str) -> Result<()> {
    let output = ToolOutput::err(format!(
        "durable launch failed: {reason}; command was NOT restarted; effects may be partial"
    ))
    .with_effects(crate::runtime::execution::Effects::Unknown);
    publish(&directory.join("finished.json"), &Reply::from(output))
}

pub(super) fn owner_main() -> Result<()> {
    let launch: Launch = serde_json::from_reader(std::io::stdin().lock().take(MAX_WIRE))?;
    private_dir(&launch.directory)?;
    let owner = ProcId::current().context("cannot establish owner kernel identity")?;
    publish(
        &launch.directory.join("owner.json"),
        &Record {
            owner,
            root: launch.root.clone(),
        },
    )?;
    let directory = launch.directory.clone();
    match run_owner(launch) {
        Ok(()) => Ok(()),
        Err(error) => {
            record_launch_failure(&directory, &format!("{error:#}"))?;
            Err(error)
        }
    }
}

fn run_owner(launch: Launch) -> Result<()> {
    ensure!(
        (1..=MAX_LIFETIME).contains(&launch.lifetime),
        "invalid lifetime"
    );
    let listener = UnixListener::bind(launch.directory.join("control.sock"))?;
    fs::set_permissions(
        launch.directory.join("control.sock"),
        fs::Permissions::from_mode(0o600),
    )?;
    listener.set_nonblocking(true)?;
    let mut ws = Workspace::new(&launch.root)?
        .with_policy(if launch.strict {
            Policy::Strict {
                deny_read: launch.deny,
                extra_write: launch.writes,
                allow_network: launch.network,
            }
        } else {
            Policy::Dev {
                deny_read: launch.deny,
                extra_write: launch.writes,
                allow_network: launch.network,
            }
        })
        .with_shell_backend(launch.backend);
    ws.additional_roots = launch.roots;
    ws.cargo_session = launch.cargo_session;
    if let Some(root) = launch.artifacts {
        ws = ws.with_artifacts(crate::storage::artifact::ArtifactStore::new(&root)?);
    }
    if launch.unsandboxed {
        ws = ws.without_sandbox();
    }
    let bg = Background {
        table: Mutex::new(Table::default()),
        interrupt: AtomicBool::new(false),
        pty_allowed: false,
    };
    let cancel = AtomicBool::new(false);
    let started = bg
        .start(&ws, &launch.args, None, &cancel)
        .map_err(anyhow::Error::msg)?;
    ensure!(!started.is_error, "command was refused: {}", started.text);
    publish(&launch.directory.join("ready.json"), &true)?;
    let end = Instant::now() + Duration::from_secs(launch.lifetime);
    let mut killed_output = None;
    loop {
        let running = {
            let mut table = bg.table();
            table.refresh();
            table.running() != 0
        };
        if !running || Instant::now() >= end {
            let output = if let Some(output) = killed_output.take() {
                output
            } else if running {
                stop(&bg, &ws, "durable lifetime expired")?
            } else {
                snapshot(&bg, &ws)?
            };
            publish(
                &launch.directory.join("finished.json"),
                &Reply::from(output),
            )?;
            break;
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_read_timeout(Some(Duration::from_millis(500)))?;
                stream.set_write_timeout(Some(Duration::from_millis(500)))?;
                let result = (|| -> Result<ToolOutput> {
                    let args: Value = serde_json::from_reader((&mut stream).take(4096))?;
                    match args["action"].as_str() {
                        Some("list") => Ok(bg.list()),
                        Some("output") => snapshot(&bg, &ws),
                        Some("kill") => {
                            let output = stop(&bg, &ws, "durable job killed")?;
                            killed_output = Some(output.clone());
                            Ok(output)
                        }
                        _ => bail!("control action is not allowed"),
                    }
                })();
                let reply = Reply::from(
                    result.unwrap_or_else(|error| ToolOutput::err(format!("{error:#}"))),
                );
                let _ = serde_json::to_writer(&mut stream, &reply);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(25))
            }
            Err(error) => return Err(error.into()),
        }
    }
    let _ = fs::remove_file(launch.directory.join("control.sock"));
    Ok(())
}

fn stop(bg: &Background, ws: &Workspace, reason: &str) -> Result<ToolOutput> {
    let mut output = snapshot(bg, ws)?;
    bg.kill(&json!({"name":"job"}))
        .map_err(anyhow::Error::msg)?;
    output.execution.process = None;
    if let Some(data) = output.data.as_mut().and_then(Value::as_object_mut) {
        data.remove("process");
    }
    output.text = format!("{reason}; effects may be partial\n{}", output.text);
    Ok(output.with_effects(crate::runtime::execution::Effects::Unknown))
}

fn snapshot(bg: &Background, ws: &Workspace) -> Result<ToolOutput> {
    // Durable reads are bounded repeatable snapshots, not a shared session cursor.
    {
        let table = bg.table();
        if let Some(proc) = table.procs.first() {
            lock(&proc.ring).cursor = 0;
        }
    }
    bg.output(ws, &json!({"name":"job"}))
        .map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests;
