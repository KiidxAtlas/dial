//! Explicit integration lanes. No discovery, installation, mock service or credential reporting.
use super::*;
use dial::benchmark::{AgentUnavailable, IntegrationMode, IntegrationOverrides};

pub(super) fn isolate(config: &Config, lane: &IntegrationOverrides) -> Result<Config> {
    lane.validate()?;
    let mut selected = isolated_config(config);
    // Built-in agents use the selected conversation model, never operator presets/role fallbacks.
    selected.agents.clear();
    selected.model_roles.clear();
    match lane.mode {
        IntegrationMode::Core => {}
        IntegrationMode::Delegation => {
            selected.experimental.work_packet_contracts = true;
        }
        IntegrationMode::Mcp => {
            selected.mcp = config
                .mcp
                .iter()
                .filter(|(name, _)| lane.mcp_servers.contains(name))
                .map(|(name, server)| (name.clone(), server.clone()))
                .collect();
            selected.sandbox.allow_network = true;
        }
        IntegrationMode::Browser => {
            selected
                .integrations
                .extend(["browser".into(), "bg".into()]);
            selected.sandbox.allow_network = true;
            selected.browser.allow_attach = false;
            selected.browser.cdp_url = None;
            selected.browser.profile = None;
            selected.browser.cmux = false;
            selected.browser.relay = false;
            selected.browser.headless = true;
            selected.browser.allow_install = false;
        }
    }
    configure_review(&mut selected);
    Ok(selected)
}

/// Explicit review experiments use only the measured model/route, never ambient coach roles.
pub(super) fn configure_review(config: &mut Config) {
    if config.verification.review.enabled {
        let coach = config.selection();
        config.model_roles.insert("coach".into(), coach);
    }
}

/// Inspect configuration/files only. Connection, handshake and browser launch are measured at run
/// time, not certified by this check. Messages intentionally omit endpoints, env, headers and args.
pub(super) fn prerequisite(
    config: &Config,
    lane: &IntegrationOverrides,
    root: &Path,
) -> Result<()> {
    let unavailable = |reason: &str| anyhow::Error::new(AgentUnavailable(reason.into()));
    match lane.mode {
        IntegrationMode::Core | IntegrationMode::Delegation => Ok(()),
        IntegrationMode::Browser => {
            if config.browser.allow_install {
                return Err(unavailable(
                    "browser unavailable: cache-only runtime support required",
                ));
            }
            tools::net::browser::prerequisites(&config.browser, root)
                .map_err(|_| unavailable("browser unavailable: installed Node, pinned runtime and external Chromium required; no installation attempted"))
        }
        IntegrationMode::Mcp => {
            for name in &lane.mcp_servers {
                let server = config
                    .mcp
                    .get(name)
                    .filter(|server| server.enabled)
                    .ok_or_else(|| {
                        unavailable("MCP unavailable: an allowlisted server is missing or disabled")
                    })?;
                if server.url.is_none() {
                    let program = server
                        .program
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("");
                    if matches!(
                        program,
                        "npx" | "npm" | "pnpm" | "bunx" | "uvx" | "pip" | "pipx"
                    ) {
                        return Err(unavailable(
                            "MCP unavailable: install/fetch launchers are not benchmark prerequisites; configure an installed executable",
                        ));
                    }
                    if !executable_exists(&server.program) {
                        return Err(unavailable(
                            "MCP unavailable: configured executable is not installed",
                        ));
                    }
                }
                if server
                    .bearer_env
                    .as_ref()
                    .is_some_and(|name| std::env::var_os(name).is_none())
                {
                    return Err(unavailable(
                        "MCP unavailable: configured bearer environment prerequisite is absent",
                    ));
                }
            }
            Ok(())
        }
    }
}

fn executable_exists(program: &Path) -> bool {
    let path = std::env::var_os("PATH");
    let pathext = cfg!(windows)
        .then(|| std::env::var_os("PATHEXT").unwrap_or_else(|| ".EXE;.COM;.BAT;.CMD".into()));
    executable_exists_in(program, path.as_deref(), pathext.as_deref())
}

// Inject environment values so tests do not mutate the process-wide PATH/PATHEXT.
// A suffix list is supplied only on Windows; Unix keeps its exact-name lookup.
fn executable_exists_in(
    program: &Path,
    path: Option<&std::ffi::OsStr>,
    pathext: Option<&std::ffi::OsStr>,
) -> bool {
    let exists = |candidate: &Path| {
        if candidate.is_file() {
            return true;
        }
        if candidate.extension().is_some() {
            return false;
        }
        pathext.is_some_and(|extensions| {
            extensions.to_string_lossy().split(';').any(|extension| {
                // Treat PATHEXT entries as suffixes, never as paths.
                let extension = extension.trim();
                extension.starts_with('.')
                    && extension.len() > 1
                    && !extension.contains(['/', '\\'])
                    && candidate.with_extension(&extension[1..]).is_file()
            })
        })
    };
    if program.is_absolute() {
        return exists(program);
    }
    if program.components().count() != 1 {
        return false;
    }
    path.is_some_and(|path| std::env::split_paths(path).any(|dir| exists(&dir.join(program))))
}

#[cfg(test)]
mod tests;
