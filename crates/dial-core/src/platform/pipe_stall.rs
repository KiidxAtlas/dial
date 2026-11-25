//! Find a command stuck on a pipe that only a detached process keeps open.
//!
//! `postfix start | tail -2` never ends: the daemon `postfix start` leaves behind inherits the
//! pipe's write end, so `tail` waits for an end of input that comes only when the daemon exits.
//! The shell running the command waits for `tail` in turn. A process that left the command's
//! process group (a daemon after `setsid`) is not stopped with the command either, so nothing
//! inside the command can end the wait. On Linux the process table in `/proc` shows this: a
//! process in the group reads its standard input from a pipe whose every writer is outside the
//! group. Other platforms report nothing.

/// A reader in the command's process group whose input pipe only detached processes hold open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PipeStall {
    pub reader: u32,
    pub reader_name: String,
    /// The detached processes holding the pipe's write end, with their command names.
    pub writers: Vec<(u32, String)>,
}

/// Readers in process group `group` stalled on pipes held open only by processes outside it.
pub fn detached_pipe_stalls(group: u32) -> Vec<PipeStall> {
    #[cfg(target_os = "linux")]
    {
        linux::stalls(group)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = group;
        Vec::new()
    }
}

/// One `/proc/<pid>/stat` entry: the command name and process group.
#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Debug, PartialEq, Eq)]
struct Stat {
    name: String,
    group: u32,
}

/// Parses the command name (field 2, which may hold spaces and parentheses) and process group
/// (field 5) of a `/proc/<pid>/stat` line.
#[cfg(any(target_os = "linux", test))]
fn parse_stat(line: &str) -> Option<Stat> {
    let open = line.find('(')?;
    let close = line.rfind(')')?;
    let name = line.get(open + 1..close)?.to_owned();
    // After the name: state (field 3), parent (4), process group (5).
    let group = line[close + 1..].split_whitespace().nth(2)?.parse().ok()?;
    Some(Stat { name, group })
}

/// Whether a `/proc/<pid>/fdinfo/<fd>` text describes a descriptor open for writing.
#[cfg(any(target_os = "linux", test))]
fn opened_for_writing(fdinfo: &str) -> bool {
    fdinfo
        .lines()
        .find_map(|line| line.strip_prefix("flags:"))
        .and_then(|flags| u32::from_str_radix(flags.trim(), 8).ok())
        // O_ACCMODE: 1 is O_WRONLY, 2 is O_RDWR.
        .is_some_and(|flags| matches!(flags & 0o3, 1 | 2))
}

/// The stall decision: a reader in `group` whose pipe has writers, none of them in `group`.
#[cfg(any(target_os = "linux", test))]
fn stalled(
    group: u32,
    readers: &[(u32, u64)],
    writers: &std::collections::BTreeMap<u64, Vec<u32>>,
    stats: &std::collections::BTreeMap<u32, Stat>,
) -> Vec<PipeStall> {
    let name = |pid: u32| {
        stats
            .get(&pid)
            .map(|stat| stat.name.clone())
            .unwrap_or_default()
    };
    readers
        .iter()
        .filter_map(|(reader, pipe)| {
            let holders = writers.get(pipe).filter(|holders| !holders.is_empty())?;
            let detached = holders
                .iter()
                .all(|pid| stats.get(pid).is_some_and(|stat| stat.group != group));
            detached.then(|| PipeStall {
                reader: *reader,
                reader_name: name(*reader),
                writers: holders.iter().map(|pid| (*pid, name(*pid))).collect(),
            })
        })
        .collect()
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{PipeStall, Stat, opened_for_writing, parse_stat, stalled};
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;

    fn pipe_inode(link: &std::path::Path) -> Option<u64> {
        let target = fs::read_link(link).ok()?;
        target
            .to_str()?
            .strip_prefix("pipe:[")?
            .strip_suffix(']')?
            .parse()
            .ok()
    }

    pub(super) fn stalls(group: u32) -> Vec<PipeStall> {
        let Ok(entries) = fs::read_dir("/proc") else {
            return Vec::new();
        };
        let pids: Vec<u32> = entries
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
            .collect();
        let stats: BTreeMap<u32, Stat> = pids
            .iter()
            .filter_map(|pid| {
                let line = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
                Some((*pid, parse_stat(&line)?))
            })
            .collect();
        let readers: Vec<(u32, u64)> = stats
            .iter()
            .filter(|(_, stat)| stat.group == group)
            .filter_map(|(pid, _)| Some((*pid, pipe_inode(format!("/proc/{pid}/fd/0").as_ref())?)))
            .collect();
        if readers.is_empty() {
            return Vec::new();
        }
        let pipes: BTreeSet<u64> = readers.iter().map(|(_, pipe)| *pipe).collect();
        let mut writers: BTreeMap<u64, Vec<u32>> = BTreeMap::new();
        for pid in stats.keys() {
            let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) else {
                continue;
            };
            for fd in fds.flatten() {
                let Some(pipe) = pipe_inode(&fd.path()).filter(|pipe| pipes.contains(pipe)) else {
                    continue;
                };
                let info = fs::read_to_string(format!(
                    "/proc/{pid}/fdinfo/{}",
                    fd.file_name().to_string_lossy()
                ))
                .unwrap_or_default();
                if opened_for_writing(&info) {
                    let holders = writers.entry(pipe).or_default();
                    if !holders.contains(pid) {
                        holders.push(*pid);
                    }
                }
            }
        }
        stalled(group, &readers, &writers, &stats)
    }
}

#[cfg(test)]
mod tests;
