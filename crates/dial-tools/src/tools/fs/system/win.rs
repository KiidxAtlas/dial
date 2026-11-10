//! Windows `sys://procs` and `sys://ports` from a Toolhelp snapshot, process parameters and the
//! IP Helper socket tables, without external tools.
use super::{Proc, Socket, render_ports, render_process, render_procs};
use crate::platform::processes::{self, Entry, Listener};
use std::collections::BTreeMap;

const PROCS_SOURCE: &str =
    "Toolhelp; other users' and elevated processes show only their image name";

fn proc(entry: &Entry) -> Proc {
    let details = processes::details(entry.pid);
    let command = details
        .as_ref()
        .and_then(|d| {
            d.command_line
                .clone()
                .filter(|line| !line.trim().is_empty())
                .or_else(|| d.executable.clone())
        })
        .unwrap_or_else(|| entry.image.clone());
    Proc {
        pid: entry.pid,
        ppid: entry.parent,
        state: if details.as_ref().is_some_and(|d| d.exited) {
            "X"
        } else {
            "-"
        }
        .into(),
        rss_bytes: details.as_ref().and_then(|d| d.working_set),
        elapsed_secs: details
            .as_ref()
            .and_then(|d| processes::age_secs(d.created)),
        command,
    }
}

fn socket(listener: Listener, images: &BTreeMap<u32, String>) -> Socket {
    Socket {
        protocol: listener.protocol,
        address: listener.address.to_string(),
        port: listener.port,
        pid: Some(listener.pid),
        program: images.get(&listener.pid).cloned(),
    }
}

fn snapshot() -> Result<Vec<Entry>, String> {
    processes::snapshot().map_err(|error| format!("the process list is unavailable: {error}"))
}

fn listeners() -> Result<Vec<Listener>, String> {
    processes::listeners().map_err(|error| format!("the socket tables are unavailable: {error}"))
}

pub(super) fn procs() -> Result<String, String> {
    let list = snapshot()?.iter().map(proc).collect();
    Ok(render_procs(list, PROCS_SOURCE, 0))
}

pub(super) fn ports() -> Result<String, String> {
    let images = snapshot()?
        .into_iter()
        .map(|entry| (entry.pid, entry.image))
        .collect();
    Ok(render_ports(
        listeners()?
            .into_iter()
            .map(|listener| socket(listener, &images))
            .collect(),
        "IP Helper socket tables",
    ))
}

pub(super) fn process(pid: u32) -> Result<String, String> {
    let list = snapshot()?;
    let entry = list
        .iter()
        .find(|entry| entry.pid == pid)
        .ok_or(format!("no visible process {pid}"))?;
    let details = processes::details(pid);
    let visible = |value: Option<String>| value.unwrap_or_else(|| "(not visible)".into());
    let children: Vec<Proc> = list
        .iter()
        .filter(|child| child.parent == pid && child.pid != pid)
        .map(proc)
        .collect();
    let images = list
        .iter()
        .map(|entry| (entry.pid, entry.image.clone()))
        .collect();
    let listening: Vec<Socket> = listeners()
        .unwrap_or_default()
        .into_iter()
        .filter(|listener| listener.pid == pid)
        .map(|listener| socket(listener, &images))
        .collect();
    Ok(render_process(
        &proc(entry),
        &[
            ("cwd", visible(details.as_ref().and_then(|d| d.cwd.clone()))),
            (
                "executable",
                visible(details.as_ref().and_then(|d| d.executable.clone())),
            ),
            ("threads", entry.threads.to_string()),
        ],
        &children,
        &listening,
        None,
    ))
}
