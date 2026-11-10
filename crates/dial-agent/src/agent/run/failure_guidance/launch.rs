//! Advisory launch recovery. Diagnostics select hints, never execution facts or permissions.
use super::*;
use crate::runtime::execution::{Completeness, ShellStdin};
use crate::tools::surface::catalog::{self, Readiness};

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(in crate::agent) enum Capability {
    Offered,
    Loadable,
    Prohibited,
    Unavailable,
}
impl Capability {
    pub(super) fn usable(&self) -> bool {
        matches!(self, Self::Offered | Self::Loadable)
    }
}

/// A snapshot of the existing exposure/readiness owners, not an authorization grant.
#[derive(Clone, Debug)]
pub(in crate::agent) struct LaunchCapabilities {
    pub bash: Capability,
    pub bg: Capability,
    pub no_pty: bool,
    pub launches_allowed: bool,
}
impl LaunchCapabilities {
    pub(in crate::agent) fn terminal_available(&self) -> bool {
        self.launches_allowed && !self.no_pty && self.bg.usable()
    }

    /// Children have a fixed, already-filtered tool surface; there is no implicit loader.
    pub(in crate::agent) fn child(
        names: &[String],
        config: &crate::config::types::Config,
        registry: &crate::tools::integrations::ext::Registry,
    ) -> Self {
        let state = |name: &str| {
            Self::child_tool(name, names, config, registry).unwrap_or(Capability::Unavailable)
        };
        Self {
            bash: state("bash"),
            bg: state("bg"),
            no_pty: config.shell.no_pty,
            launches_allowed: true,
        }
    }

    pub(in crate::agent) fn child_tool(
        name: &str,
        names: &[String],
        config: &crate::config::types::Config,
        registry: &crate::tools::integrations::ext::Registry,
    ) -> Option<Capability> {
        let offered = names.iter().any(|tool| tool == name);
        let known = offered
            || crate::config::tools::BUILTIN.contains(&name)
            || registry.contains(name)
            || config.tools.contains_key(name);
        known.then(|| {
            capability(
                true,
                offered,
                offered,
                false,
                &catalog::readiness(name, config, registry),
            )
        })
    }
}

fn capability(
    present: bool,
    eligible: bool,
    offered: bool,
    loader: bool,
    readiness: &Readiness,
) -> Capability {
    if !eligible {
        Capability::Prohibited
    } else if !present || !readiness.automatic_candidate() {
        Capability::Unavailable
    } else if offered {
        Capability::Offered
    } else if loader {
        Capability::Loadable
    } else {
        Capability::Unavailable
    }
}

impl crate::agent::Agent<'_> {
    /// A current surface/readiness snapshot. A loadable tool still needs a later schema and
    /// ordinary dispatch authorization; a catalog entry alone grants neither.
    pub(in crate::agent) fn recovery_capability(
        &self,
        name: &str,
        offered: &[String],
    ) -> Option<Capability> {
        self.catalog().iter().any(|tool| tool == name).then(|| {
            // `tool_load` is folded into `tool_search` (`names`); either offered name loads.
            let loader = offered
                .iter()
                .any(|tool| matches!(tool.as_str(), "tool_search" | "tool_load"))
                && self.tool_eligible("tool_search");
            capability(
                true,
                self.tool_eligible(name)
                    && !self.hermetic
                    && self.config.active_model_profile().is_none_or(|profile| {
                        profile.capabilities.tools != Some(false)
                            && profile
                                .tools
                                .as_ref()
                                .is_none_or(|tools| tools.iter().any(|tool| tool == name))
                    }),
                offered.iter().any(|tool| tool == name),
                loader,
                &catalog::readiness(name, self.config, &self.ext),
            )
        })
    }

    pub(in crate::agent) fn launch_recovery_capabilities(
        &self,
        offered: &[String],
    ) -> LaunchCapabilities {
        LaunchCapabilities {
            bash: self
                .recovery_capability("bash", offered)
                .unwrap_or(Capability::Unavailable),
            bg: self
                .recovery_capability("bg", offered)
                .unwrap_or(Capability::Unavailable),
            no_pty: self.config.shell.no_pty,
            launches_allowed: !self.plan_mode,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Diagnostic {
    Terminal,
    Input,
}

/// Bounded runtime diagnostics only, not arbitrary tool output or an execution grant.
fn diagnostic(output: &ToolOutput) -> Option<Diagnostic> {
    if output.text.len() > 32 * 1024
        || output.text.contains("```")
        || output.execution.output.as_ref().is_some_and(|integrity| {
            [
                integrity.capture,
                integrity.retention,
                integrity.presentation,
            ]
            .contains(&Completeness::Partial)
        })
    {
        return None;
    }
    output.text.lines().find_map(|line| {
        let line = line.trim();
        match line {
            "_curses.error: setupterm: could not find terminal"
            | "setupterm: could not find terminal"
            | "Error opening terminal: unknown."
            | "_curses.error: cbreak() returned ERR"
            | "_curses.error: nocbreak() returned ERR"
            | "termios.error: (25, 'Inappropriate ioctl for device')"
            | "tcgetattr: Inappropriate ioctl for device" => return Some(Diagnostic::Terminal),
            "EOFError: EOF when reading a line" | "EOFError: EOF when reading input" => {
                return Some(Diagnostic::Input);
            }
            _ => {}
        }
        // Error-prefixed requirement statements generalize across programs without treating
        // quoted documentation, arbitrary mentions or a command name as proof of a prerequisite.
        let lower = line.to_ascii_lowercase();
        let message = lower.strip_prefix("error:")?.trim();
        if message.contains('"') || message.starts_with('\'') {
            return None;
        }
        let words: Vec<_> = message
            .split(|c: char| !c.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .collect();
        let negated = words.windows(2).any(|pair| {
            matches!(
                pair,
                ["not", "required" | "require" | "requires"] | ["never", "required"]
            )
        });
        let terminal_requirement = !negated
            && words.iter().enumerate().any(|(index, word)| {
                if !matches!(*word, "terminal" | "tty") {
                    return false;
                }
                let before = &words[index.saturating_sub(5)..index];
                before
                    .iter()
                    .any(|word| matches!(*word, "requires" | "required" | "require" | "needs"))
                    || before.ends_with(&["not", "a"])
                    || words.get(index + 1..).is_some_and(|after| {
                        after.starts_with(&["required"]) || after.starts_with(&["is", "required"])
                    })
            });
        if terminal_requirement {
            Some(Diagnostic::Terminal)
        } else if message.contains("stdin")
            && (message.contains("eof") || message.contains("closed"))
            && (message.contains("input") || message.contains("read"))
        {
            Some(Diagnostic::Input)
        } else {
            None
        }
    })
}

fn diagnostic_fact(hint: Diagnostic) -> &'static str {
    match hint {
        Diagnostic::Terminal => {
            "This launch had no allocated terminal; the diagnostic suggests a terminal or terminal-environment prerequisite, not proof that this host cannot support it."
        }
        Diagnostic::Input => {
            "This launch had closed stdin; the EOF diagnostic suggests missing input, not a terminal requirement or proof that interaction is unsupported."
        }
    }
}

/// Shared by foreground and worker settlement. Prose never mutates status or authorizes replay.
pub(in crate::agent) fn shell_launch_recovery(
    call: &ToolCall,
    output: &mut ToolOutput,
    capabilities: &LaunchCapabilities,
) -> Option<String> {
    if !matches!(call.name.as_str(), "bash" | "bg")
        || output.execution.effects == Effects::Unknown
        || !matches!(
            ToolStatus::from_output(output),
            ToolStatus::Ok | ToolStatus::Error
        )
    {
        return None;
    }
    if call.name == "bg"
        && !matches!(
            call.arguments["action"].as_str(),
            Some("start" | "output" | "wait")
        )
    {
        return None;
    }
    let launch = output.execution.shell_launch.as_ref()?;
    let process = output.execution.process.as_ref()?;
    if !process.started || process.cancelled || process.timed_out || process.signal.is_some() {
        return None;
    }
    let quoting_output = call.arguments["command"].as_str().is_some_and(|command| {
        crate::agent::safety::approval::shell_words(command).is_some_and(|words| {
            words.first().is_some_and(|word| {
                matches!(
                    word.as_str(),
                    "echo" | "printf" | "cat" | "grep" | "rg" | "sed" | "head" | "tail" | "man"
                )
            })
        })
    });
    let hint = (process.output_complete && !quoting_output)
        .then(|| diagnostic(output))
        .flatten()
        .filter(|hint| match hint {
            Diagnostic::Terminal => !launch.terminal_allocated,
            Diagnostic::Input => launch.stdin == ShellStdin::Eof,
        });
    if let Some(job) = &launch.job {
        let failed = process.exit_code.is_some_and(|code| code != 0);
        if process.exit_code.is_none() || (call.name == "bg" && failed) {
            let durable = call.name == "bg" && call.arguments["persist"] == true;
            let mut identity = serde_json::json!({"name": job});
            if durable {
                identity["persist"] = true.into();
            }
            let candidate = Candidate::native("bg", capabilities.bg, {
                let mut args = identity.clone();
                args["action"] = "output".into();
                args
            });
            let next = if candidate.arguments.is_some() {
                let load = candidate.load_guidance();
                let actions = if durable { "kill" } else { "input/resize/kill" };
                format!(
                    "{load}Use permitted bg output/wait with {identity}; {actions} only when appropriate and authorized. A running job cannot acquire a terminal or reopen closed stdin by replaying its launch."
                )
            } else {
                "bg observation/control is not currently available under this tool surface. Report that limit; do not bypass the allowlist or launch a replacement.".to_owned()
            };
            let required = "Observe the existing job before deciding on any corrected launch.";
            let mut advisory = Advisory::new(
                call,
                "managed_job_observation",
                "execution.process + execution.shell_launch",
                required,
            );
            advisory.candidates.push(candidate);
            let advice = policy::managed_launch_observation(failed, &next);
            let text = match hint {
                Some(hint) => format!(
                    "{}{}",
                    policy::launch_diagnostic_advice(diagnostic_fact(hint), required),
                    advice
                ),
                None => advice,
            };
            advisory.attach(output);
            return Some(text);
        }
    }
    let hint = hint?;
    let fact = diagnostic_fact(hint);
    let required = "Inspect prior effects and correct the supported input/terminal prerequisite before a permitted launch; no automatic replay.";
    let mut advisory = Advisory::new(
        call,
        match hint {
            Diagnostic::Terminal => "terminal_prerequisite",
            Diagnostic::Input => "input_prerequisite",
        },
        "execution.process + execution.shell_launch + diagnostic",
        required,
    );
    let mut next = String::new();
    if !capabilities.launches_allowed {
        next.push_str("Launching a replacement is prohibited in the current mode. Report this boundary; do not switch tools to bypass it.");
        advisory
            .candidates
            .push(Candidate::new("bg", Capability::Prohibited));
    } else if hint == Diagnostic::Terminal && capabilities.no_pty {
        next.push_str("PTY allocation is disabled by shell.no_pty. Do not bypass that setting; report the required terminal prerequisite.");
        advisory
            .candidates
            .push(Candidate::new("bg", Capability::Prohibited));
    } else if !capabilities.bg.usable() {
        next.push_str(match capabilities.bg {
            Capability::Prohibited => "Managed bg is prohibited by the current tool allowlist or mode. Do not load or switch tools to bypass it; report that limit.",
            _ => "Managed bg is unavailable or has an unmet prerequisite. Inspect the named capability prerequisite or report the gap; do not infer that all interaction is unsupported and do not install dependencies without approval.",
        });
        advisory
            .candidates
            .push(Candidate::new("bg", capabilities.bg));
    } else if let Some(command) = call.arguments["command"].as_str() {
        let mut args = json!({"command": command});
        if let Some(cwd) = call.arguments.get("cwd") {
            args["cwd"] = cwd.clone();
        }
        let (tool, availability) =
            if hint == Diagnostic::Terminal && capabilities.bash == Capability::Offered {
                args["pty"] = true.into();
                ("bash", capabilities.bash)
            } else {
                args["action"] = "start".into();
                if hint == Diagnostic::Terminal {
                    args["pty"] = true.into();
                } else {
                    args["stdin"] = true.into();
                }
                ("bg", capabilities.bg)
            };
        let candidate = Candidate::native(tool, availability, args);
        if let Some(args) = &candidate.arguments {
            next.push_str(&candidate.load_guidance());
            next.push_str(&format!("If the requested run is still needed, after inspecting prior effects and correcting the supported cause, a permitted {tool} launch would preserve the exact original command/cwd: {args}. "));
            next.push_str("The suggestion omits timeout and session because PTY/managed starts do not accept those foreground options; state from an embedded shell session cannot migrate to the new process. Re-establish required state explicitly, without assuming it transferred. ");
            if hint == Diagnostic::Input {
                next.push_str("Use bg input/close_stdin for the pipe; stdin:true does not allocate a terminal. ");
            }
            next.push_str(
                "Do not replay automatically or treat this suggestion as an execution grant.",
            );
        } else {
            next.push_str("Inspect the original launch: its command/cwd cannot form a bounded schema-valid proposal. No replay arguments are suggested.");
        }
        advisory.candidates.push(candidate);
    } else {
        next.push_str("Inspect this job's original launch and effects before deciding on a corrected run; this observation contains no original command/cwd to replay.");
    }
    advisory.required_observation = &next;
    advisory.attach(output);
    Some(policy::launch_diagnostic_advice(fact, &next))
}

#[cfg(test)]
mod tests;
