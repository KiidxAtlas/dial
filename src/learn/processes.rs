//! Live Dial process inventory. Commands stay private; controls use a refreshed PID identity.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, path::Path, process::Command};

pub use crate::storage::process_record::Record;

#[derive(Debug)]
struct Process {
    pid: i32,
    parent: i32,
    group: i32,
    cpu: f64,
    memory: u64,
    started: String,
    command: String,
}
impl Process {
    fn identity(&self) -> String {
        format!(
            "{:x}",
            Sha256::digest(format!("{}\n{}\n{}", self.pid, self.started, self.command))
        )
    }
}

fn parse(output: &str) -> Vec<Process> {
    output
        .lines()
        .filter_map(|line| {
            let mut rest = line.trim();
            let mut next = || {
                let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                let token = &rest[..end];
                rest = rest[end..].trim_start();
                token
            };
            let pid = next().parse().ok()?;
            let parent = next().parse().ok()?;
            let group = next().parse().ok()?;
            let cpu = next().parse().ok()?;
            let memory = next().parse::<u64>().ok()?.saturating_mul(1024);
            let started = (0..5)
                .map(|_| next().to_owned())
                .collect::<Vec<_>>()
                .join(" ");
            if rest.is_empty() || pid <= 1 {
                return None;
            }
            Some(Process {
                pid,
                parent,
                group,
                cpu,
                memory,
                started,
                command: rest.to_owned(),
            })
        })
        .collect()
}
fn read() -> Result<Vec<Process>> {
    let output = Command::new("ps")
        .env("LC_ALL", "C")
        .args(["-axo", "pid=,ppid=,pgid=,%cpu=,rss=,lstart=,args="])
        .output()
        .context("reading live processes")?;
    ensure!(
        output.status.success(),
        "live process inspection is unavailable"
    );
    Ok(parse(&String::from_utf8_lossy(&output.stdout)))
}

/// Tests that inspect live processes cannot run where `ps` is denied (for example inside a sandbox).
#[cfg(test)]
pub(crate) fn live_inspection_unavailable() -> bool {
    let unavailable = read().is_err();
    if unavailable {
        eprintln!("skipping: live process inspection is unavailable in this environment");
    }
    unavailable
}

fn classify(user: &Path, process: &Process, all: &[Process]) -> Option<(&'static str, String)> {
    let command = &process.command;
    let program = command.split_whitespace().next()?;
    let managed_python = Path::new(program).starts_with(user.join("python"));
    let laya_python = Path::new(program).starts_with(user.join("laya"));
    let model = command
        .split_whitespace()
        .find_map(|word| word.strip_prefix("--model="))
        .map(str::to_owned)
        .unwrap_or_default();
    if managed_python
        && (command.contains("mlx_lm.server")
            || command.contains("Dial's MLX server")
            || command.contains("dial_steering"))
    {
        return Some(("models", model));
    }
    if laya_python && command.contains("dial_laya_server:app") {
        return Some(("services", "Laya decisions".into()));
    }
    let shared_laya_python = std::env::var_os("HOME").is_some_and(|home| {
        Path::new(program).starts_with(Path::new(&home).join(".omp/agent/laya-venv"))
    });
    if shared_laya_python
        && command
            .split_whitespace()
            .any(|word| word == "laya_server:app")
    {
        return Some(("services", "Laya (OMP/shared)".into()));
    }
    if managed_python && command.contains("dial_worker.py") {
        return Some(("models", "Model worker".into()));
    }
    if command.contains("__dial_kernel__") && command.contains("Trusted JSON-lines kernel driver") {
        return Some(("tools", "Python kernel".into()));
    }
    // Descendants of a live Dial session include kernels, MCP/LSP servers and shell tools.
    // Other Python programs need a Dial ancestor; shared OMP Laya is handled explicitly above.
    let mut parent = process.parent;
    for _ in 0..16 {
        let ancestor = all.iter().find(|p| p.pid == parent)?;
        if Path::new(ancestor.command.split_whitespace().next()?)
            .file_name()
            .is_some_and(|name| name == "dial")
        {
            return Some((
                "tools",
                Path::new(program)
                    .file_name()?
                    .to_string_lossy()
                    .into_owned(),
            ));
        }
        parent = ancestor.parent;
    }
    None
}
fn inventory(user: &Path, all: &[Process]) -> Vec<Value> {
    let mut rows = Vec::new();
    for process in all {
        if process.pid == std::process::id() as i32 {
            continue;
        }
        // Registration supplies startup/activity and model identity even with a custom Python path.
        let record = fs::read(
            user.join("compute-processes")
                .join(format!("{}.json", process.pid)),
        )
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(|value| value["pid"] == process.pid && value["parent_pid"] == process.parent);
        let classified = classify(user, process, all);
        if record.is_none() && classified.is_none() {
            continue;
        }
        // Dial sessions and detached schedulers have separate controls; don't terminate them as tools.
        let program = process.command.split_whitespace().next().unwrap_or("");
        if Path::new(program)
            .file_name()
            .is_some_and(|name| name == "dial" || name == "ps")
        {
            continue;
        }
        let mut row = record.unwrap_or_else(|| {
            let (kind, model) = classified.unwrap();
            json!({"kind":kind,"model":model,"phase":"running","project":"","state":""})
        });
        row["pid"] = json!(process.pid);
        row["parent_pid"] = json!(process.parent);
        row["cpu_percent"] = json!(process.cpu);
        row["memory_bytes"] = json!(process.memory);
        row["identity"] = json!(process.identity());
        row["orphaned"] = json!(process.parent == 1 && row["kind"] != "services");
        rows.push(row);
    }
    rows.sort_by_key(|row| std::cmp::Reverse(row["memory_bytes"].as_u64().unwrap_or(0)));
    rows
}
#[cfg(target_os = "macos")]
fn footprint(pid: i32) -> Option<u64> {
    // RSS misses compressed and Metal-backed allocations. Use Darwin's process footprint.
    let mut usage = std::mem::MaybeUninit::<libc::rusage_info_v2>::zeroed();
    let result =
        unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V2, usage.as_mut_ptr().cast()) };
    (result == 0).then(|| unsafe { usage.assume_init().ri_phys_footprint })
}
#[cfg(not(target_os = "macos"))]
fn footprint(_pid: i32) -> Option<u64> {
    None
}

pub struct UsageSnapshot {
    pub processes: Vec<Value>,
    pub session_memory_bytes: u64,
    pub session_count: usize,
}

pub fn snapshot_with_usage(user: &Path) -> Result<UsageSnapshot> {
    let all = read()?;
    let mut processes = inventory(user, &all);
    for row in &mut processes {
        row["resident_memory_bytes"] = row["memory_bytes"].clone();
        if let Some(bytes) = row["pid"].as_i64().and_then(|pid| footprint(pid as i32)) {
            row["memory_bytes"] = json!(bytes);
            row["memory_metric"] = json!("footprint");
        } else {
            row["memory_metric"] = json!("rss");
        }
        row["shared"] = json!(row["model"] == "Laya (OMP/shared)");
    }
    processes.sort_by_key(|row| std::cmp::Reverse(row["memory_bytes"].as_u64().unwrap_or(0)));
    let sessions: Vec<_> = all
        .iter()
        .filter(|process| {
            process
                .command
                .split_whitespace()
                .next()
                .and_then(|program| Path::new(program).file_name())
                .is_some_and(|name| name == "dial")
        })
        .collect();
    let session_memory_bytes = sessions.iter().fold(0_u64, |sum, process| {
        sum.saturating_add(footprint(process.pid).unwrap_or(process.memory))
    });
    Ok(UsageSnapshot {
        processes,
        session_memory_bytes,
        session_count: sessions.len(),
    })
}

pub fn snapshot(user: &Path) -> Result<Vec<Value>> {
    Ok(snapshot_with_usage(user)?.processes)
}

pub fn stop(user: &Path, pid: i32, identity: &str) -> Result<String> {
    ensure!(
        pid > 1 && pid != std::process::id() as i32,
        "cannot stop this process"
    );
    let all = read()?;
    let process = all
        .iter()
        .find(|p| p.pid == pid)
        .context("process already exited")?;
    ensure!(
        process.identity() == identity,
        "process changed; refresh and select it again"
    );
    let row = inventory(user, &all)
        .into_iter()
        .find(|row| row["pid"] == pid)
        .context("process is no longer managed by Compute")?;
    // Only signal a private child group. Never signal the foreground session's shared group.
    let result = unsafe {
        if process.group == pid {
            libc::killpg(pid, libc::SIGTERM)
        } else {
            libc::kill(pid, libc::SIGTERM)
        }
    };
    ensure!(
        result == 0,
        "could not stop process: {}",
        std::io::Error::last_os_error()
    );
    Ok(if row["model"] == "Laya (OMP/shared)" {
        "Shared Laya stop requested. OMP also uses this service and may restart it on its next request.".into()
    } else {
        "Process stop requested. Models reload on their next use; saved model files are preserved."
            .into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_reads_real_memory_footprint_and_handles_an_exited_pid() {
        assert!(footprint(std::process::id() as i32).unwrap() > 0);
        assert_eq!(footprint(i32::MAX), None);
    }
    #[test]
    fn shared_omp_laya_is_visible_and_unrelated_services_are_excluded() {
        let user = tempfile::tempdir().unwrap();
        let home = std::env::var_os("HOME").unwrap();
        let python = Path::new(&home).join(".omp/agent/laya-venv/bin/python");
        let all = parse(&format!(
            "42 1 42 1.0 307200 Tue Oct 6 08:00:00 2026 {} -m uvicorn laya_server:app --port 8001\n43 1 43 1.0 100 Tue Oct 6 08:00:00 2026 {} -m uvicorn other:app\n44 1 44 1.0 100 Tue Oct 6 08:00:00 2026 /other/laya-venv/bin/python -m uvicorn laya_server:app\n",
            python.display(),
            python.display()
        ));
        let rows = inventory(user.path(), &all);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["pid"], 42);
        assert_eq!(rows[0]["kind"], "services");
        assert_eq!(rows[0]["model"], "Laya (OMP/shared)");
        assert_eq!(rows[0]["orphaned"], false);
    }

    #[test]
    fn inventory_finds_old_orphaned_models_services_and_dial_children_only() {
        let home = tempfile::tempdir().unwrap();
        let text = format!(
            "42 1 42 0.0 35651584 Tue Oct 6 08:00:00 2026 {}/python/.venv/bin/python -m mlx_lm.server --model=big\n43 1 43 1.0 100 Tue Oct 6 08:00:00 2026 {}/laya/venv/bin/python -m uvicorn dial_laya_server:app\n44 1 44 2.0 100 Tue Oct 6 08:00:00 2026 /other/python -m uvicorn other:app\n45 1 45 0.0 100 Tue Oct 6 08:00:00 2026 /bin/dial\n46 45 46 8.0 100 Tue Oct 6 08:00:00 2026 /bin/python -c tool\n",
            home.path().display(),
            home.path().display()
        );
        let all = parse(&text);
        let rows = inventory(home.path(), &all);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["model"], "big");
        assert_eq!(rows[0]["memory_bytes"], 34_u64 * 1024 * 1024 * 1024);
        assert_eq!(rows[0]["orphaned"], true);
        assert!(rows.iter().any(|row| row["kind"] == "services"));
        assert!(
            rows.iter()
                .any(|row| row["pid"] == 46 && row["kind"] == "tools")
        );
        assert!(!rows.iter().any(|row| row["pid"] == 44));
    }
    #[test]
    fn registration_exposes_loading_for_custom_python_and_cleans_up() {
        let home = tempfile::tempdir().unwrap();
        let mut record = Record::new(
            home.path(),
            42,
            "models",
            "test",
            Path::new("project"),
            Path::new("state"),
            "loading",
        )
        .unwrap();
        let all = parse(&format!(
            "42 {} 42 8.0 1024 Tue Oct 6 08:00:00 2026 /custom/python -c server",
            std::process::id()
        ));
        assert_eq!(inventory(home.path(), &all)[0]["phase"], "loading");
        record.phase("loaded").unwrap();
        assert_eq!(inventory(home.path(), &all)[0]["phase"], "loaded");
        let path = home.path().join("compute-processes/42.json");
        drop(record);
        assert!(!path.exists());
    }
    #[test]
    fn identity_changes_when_a_pid_is_reused_or_command_changes() {
        let first = parse("42 1 42 0 100 Tue Oct 6 08:00:00 2026 /bin/python server");
        let later = parse("42 1 42 0 100 Tue Oct 6 08:01:00 2026 /bin/python server");
        assert_ne!(first[0].identity(), later[0].identity());
    }

    #[test]
    fn stop_rechecks_identity_and_terminates_only_the_selected_child() {
        if live_inspection_unavailable() {
            return;
        }
        use std::os::unix::process::CommandExt;
        let home = tempfile::tempdir().unwrap();
        let mut child = Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let _record = Record::new(
            home.path(),
            child.id(),
            "models",
            "test-model",
            Path::new("project"),
            Path::new("state"),
            "loading",
        )
        .unwrap();
        let row = snapshot(home.path())
            .unwrap()
            .into_iter()
            .find(|row| row["pid"] == child.id())
            .unwrap();
        assert!(
            stop(home.path(), child.id() as i32, "stale")
                .unwrap_err()
                .to_string()
                .contains("process changed")
        );
        assert!(child.try_wait().unwrap().is_none());
        stop(
            home.path(),
            child.id() as i32,
            row["identity"].as_str().unwrap(),
        )
        .unwrap();
        assert!(!child.wait().unwrap().success());
        assert!(
            !snapshot(home.path())
                .unwrap()
                .iter()
                .any(|row| row["pid"] == child.id())
        );
    }
}
