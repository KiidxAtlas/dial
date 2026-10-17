//! Approved remote execution and SSH URL support.
use crate::config::tools::ssh::SshConfig;

mod config;
mod connection;
pub(crate) mod process;
pub mod url;

use super::ext::{Tool, ToolContext};
use crate::config::types::{Config, SshHostConfig};
use crate::tools::{ToolOutput, opt_u64, req_str, tool_schema};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

pub use config::resolve as resolve_hosts;
pub use connection::{Os, PosixShell, Target};

pub(super) struct Settings {
    pub(super) user_dir: std::path::PathBuf,
    pub(super) config: SshConfig,
}

pub(crate) fn user_dir() -> Result<std::path::PathBuf, String> {
    std::env::var_os("DIAL_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".dial"))
        })
        .ok_or_else(|| "HOME is unset; cannot access the SSH connection state".to_owned())
}

/// Every addressable host as a connection target: `[ssh_hosts]` entries carry their HostName,
/// user, port, and key and win over `~/.ssh/config` aliases; any other name is left to OpenSSH.
pub fn host_targets(
    home: &std::path::Path,
    configured: &BTreeMap<String, SshHostConfig>,
) -> BTreeMap<String, Target> {
    resolve_hosts(home, configured)
        .into_iter()
        .map(|host| {
            let target = match host.hostname {
                Some(hostname) => Target {
                    name: host.alias.clone(),
                    host: hostname,
                    user: host.user,
                    port: host.port,
                    key: host.identity_file.map(std::path::PathBuf::from),
                    compat: None,
                },
                None => Target::opaque(&host.alias),
            };
            (host.alias, target)
        })
        .collect()
}

fn target_for(hosts: &BTreeMap<String, Target>, name: &str) -> Target {
    hosts
        .get(name)
        .cloned()
        .unwrap_or_else(|| Target::opaque(name))
}

/// Read a remote URL through the same bounded, non-interactive SSH process path as the SSH tool.
pub(crate) fn read_url(
    input: &str,
    user_dir: &std::path::Path,
    ssh: &SshConfig,
    hosts: &BTreeMap<String, Target>,
    cancel: &AtomicBool,
) -> Option<Result<String, String>> {
    if !url::is_ssh_url(input) {
        return None;
    }
    Some((|| {
        let url = match url::parse(input)? {
            url::Parsed::Url(url) => url,
            url::Parsed::Index => return Err("ssh:// host index is not a file".into()),
        };
        let mut target = target_for(hosts, &url.host);
        if url.user.is_some() {
            target.user = url.user.clone();
        }
        if url.port.is_some() {
            target.port = url.port;
        }
        let settings = Settings {
            user_dir: user_dir.to_path_buf(),
            config: ssh.clone(),
        };
        let shell = transfer_shell(&target, &settings, cancel)?;
        let command = shell.wrap(&format!("cat -- {}", url.path.shell_word()));
        let done = connection::capture(
            &target,
            &settings,
            &command,
            Duration::from_secs(30),
            1024 * 1024,
            cancel,
        )?;
        if !done.success() {
            return Err(done.failure(&format!("reading {input}"), Duration::from_secs(30)));
        }
        if done.stdout_total > done.stdout.len() {
            return Err("remote file exceeds 1 MiB".into());
        }
        Ok(String::from_utf8_lossy(&done.stdout).into_owned())
    })())
}

pub(crate) fn write_url(
    input: &str,
    user_dir: &std::path::Path,
    ssh: &SshConfig,
    hosts: &BTreeMap<String, Target>,
    bytes: &[u8],
    cancel: &AtomicBool,
) -> Result<(), String> {
    if bytes.len() > 1024 * 1024 {
        return Err("SSH URL writes are limited to 1 MiB".into());
    }
    let url = match url::parse(input)? {
        url::Parsed::Url(url) => url,
        url::Parsed::Index => return Err("ssh:// host index is not a file".into()),
    };
    let mut target = target_for(hosts, &url.host);
    if url.user.is_some() {
        target.user = url.user.clone();
    }
    if url.port.is_some() {
        target.port = url.port;
    }
    let settings = Settings {
        user_dir: user_dir.to_path_buf(),
        config: ssh.clone(),
    };
    let shell = transfer_shell(&target, &settings, cancel)?;
    let path = url.path.shell_word();
    // Stage to a sibling temp file so a failed transfer leaves `dest` untouched. An existing
    // destination (file or symlink) is then overwritten in place, keeping its mode, owner and
    // symlink; `mv` would replace the link itself and reset the mode to the 0600 of the temp file.
    let script = format!(
        "dest={path}; tmp=\"${{dest}}.dial-tmp-$$\"; umask 077; trap 'rm -f -- \"$tmp\"' EXIT HUP INT TERM; cat > \"$tmp\" && if [ -e \"$dest\" ] || [ -L \"$dest\" ]; then cat \"$tmp\" > \"$dest\"; else mv -f -- \"$tmp\" \"$dest\"; fi"
    );
    let command = shell.wrap(&script);
    let args = connection::remote_args(&target, &settings, &command, true)?;
    let done = process::run(
        connection::ssh_command(&args),
        Some(bytes.to_vec()),
        Duration::from_secs(30),
        64 * 1024,
        cancel,
        &mut |_, _| {},
    )?;
    if !done.success() {
        return Err(done.failure(&format!("writing {input}"), Duration::from_secs(30)));
    }
    Ok(())
}

/// Verified connection plus the POSIX shell that runs transfer snippets. A host without one
/// (Windows without a compat layer) is refused rather than sent a script its shell cannot parse.
fn transfer_shell(
    target: &Target,
    settings: &Settings,
    cancel: &AtomicBool,
) -> Result<PosixShell, String> {
    connection::ensure_connection(target, settings, cancel)?;
    let info = connection::host_info(target, settings, cancel)?;
    info.transfer_shell.ok_or_else(|| match info.os {
        Os::Windows => format!(
            "SSH URL transfers are not supported on Windows host {} without a POSIX shell",
            target.name
        ),
        _ => format!(
            "no verified POSIX shell for SSH URL transfers on {}",
            target.name
        ),
    })
}
pub fn tools(config: &Config) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(SshTool {
        settings: Settings {
            user_dir: config.user_dir.clone(),
            config: config.ssh.clone(),
        },
        hosts: config.ssh_hosts.clone(),
    })]
}
struct SshTool {
    settings: Settings,
    /// `[ssh_hosts]` entries; these win over `~/.ssh/config` aliases with the same name.
    hosts: BTreeMap<String, SshHostConfig>,
}
impl SshTool {
    /// The connection target for a host name: a configured entry carries its own HostName, user,
    /// port, and key; any other name is an alias that OpenSSH resolves itself.
    fn target(&self, name: &str) -> Target {
        match self.hosts.get(name) {
            Some(entry) => Target {
                name: name.to_owned(),
                host: entry.host.clone(),
                user: entry.user.clone(),
                port: entry.port,
                key: entry.key.as_ref().map(std::path::PathBuf::from),
                compat: None,
            },
            None => Target::opaque(name),
        }
    }
}
impl Tool for SshTool {
    fn name(&self) -> &str {
        "ssh"
    }
    fn schema(&self) -> Value {
        tool_schema(
            "ssh",
            "Inspect configured SSH hosts or run an approved remote command outside the workspace sandbox.",
            json!({"action":{"type":"string","enum":["hosts","run","close"]},"host":{"type":"string","description":"SSH host alias or hostname (run, close)"},"command":{"type":"string","description":"Remote command (run)"},"timeout":{"type":"integer","minimum":1,"maximum":3600}}),
            &["action"],
        )
    }
    fn needs_approval(&self, args: &Value) -> bool {
        !matches!(args["action"].as_str(), Some("hosts" | "close"))
    }
    fn may_modify_workspace(&self, _: &Value) -> bool {
        false
    }
    fn call(&self, ctx: &ToolContext, args: &Value) -> ToolOutput {
        self.call_streaming(ctx, args, &|_| {}, &AtomicBool::new(false))
    }
    fn call_streaming(
        &self,
        _ctx: &ToolContext,
        args: &Value,
        on_event: &(dyn Fn(super::ext::ToolEvent) + Sync),
        cancel: &AtomicBool,
    ) -> ToolOutput {
        let result = (|| -> Result<String, String> {
            let action = req_str(args, "action")?;
            if action == "hosts" {
                let home = self
                    .settings
                    .user_dir
                    .parent()
                    .ok_or("SSH user directory has no parent home")?;
                let hosts = config::resolve(home, &self.hosts);
                if hosts.is_empty() {
                    return Ok("No SSH hosts found in ~/.ssh/config or [ssh_hosts].".into());
                }
                let mut lines: Vec<String> = hosts
                    .iter()
                    .map(|host| {
                        let mut detail = host.alias.clone();
                        if let Some(hostname) = &host.hostname {
                            detail.push_str(&format!(" -> {hostname}"));
                        }
                        if let Some(user) = &host.user {
                            detail.push_str(&format!(" user={user}"));
                        }
                        if let Some(port) = host.port {
                            detail.push_str(&format!(" port={port}"));
                        }
                        if let Some(info) =
                            connection::cached_info(&self.target(&host.alias), &self.settings)
                        {
                            detail.push_str(&format!(" [{}]", info.label()));
                        }
                        detail
                    })
                    .collect();
                let open = connection::active();
                if !open.is_empty() {
                    lines.push(format!("open masters: {}", open.join(", ")));
                }
                return Ok(lines.join("\n"));
            }
            if action == "close" {
                let host = req_str(args, "host")?;
                let closed = connection::close(&self.settings, &self.target(host))?;
                return Ok(if closed {
                    format!("closed SSH master for {host}")
                } else {
                    format!("no open SSH master for {host}")
                });
            }
            if action != "run" {
                return Err("action must be hosts, run, or close".into());
            }
            let host = req_str(args, "host")?;
            let command = req_str(args, "command")?;
            if command.len() > 64 * 1024 {
                return Err("SSH command exceeds 64 KiB".into());
            }
            let timeout = opt_u64(args, "timeout")?.unwrap_or(300).clamp(1, 3600);
            let target = self.target(host);
            connection::ensure_connection(&target, &self.settings, cancel)?;
            let info = connection::host_info(&target, &self.settings, cancel)?;
            let command = match (info.compat_enabled, info.compat_shell) {
                (true, Some(shell)) => shell.wrap(command),
                _ => command.to_owned(),
            };
            let argv = connection::remote_args(&target, &self.settings, &command, false)?;
            let child = connection::ssh_command(&argv);
            let mut stream = |kind: process::Stream, bytes: &[u8]| {
                on_event(super::ext::ToolEvent {
                    kind: "output".into(),
                    name: "ssh".into(),
                    text: format!("{kind:?}: {}", String::from_utf8_lossy(bytes)),
                    artifact: None,
                });
            };
            let done = process::run(
                child,
                None,
                Duration::from_secs(timeout),
                1024 * 1024,
                cancel,
                &mut stream,
            )?;
            if !done.success() {
                return Err(done.failure(
                    &format!("SSH command on {host}"),
                    Duration::from_secs(timeout),
                ));
            }
            let mut out = String::from_utf8_lossy(&done.stdout).into_owned();
            if done.stdout_total > done.stdout.len() {
                out.push_str(&format!(
                    "\n[output truncated; {} bytes omitted]",
                    done.stdout_total - done.stdout.len()
                ));
            }
            Ok(out)
        })();
        match result {
            Ok(text) => ToolOutput::structured(json!({"text":text})),
            Err(error) => ToolOutput::err(error),
        }
    }
}
