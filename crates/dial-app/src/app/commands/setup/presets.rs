//! Opinionated settings bundles, not a new network or approval boundary.
//! Only already-selected models are used; previews never contact a provider.
use super::*;
use crate::config::settings_store;
use crate::config::types::Approval;
use crate::llm::model_config::ModelSelection;
use anyhow::{Context, ensure};

const TEXT_ROLES: &[&str] = &[
    "small", "task", "coach", "tester", "planner", "memory", "judge",
];
const USAGE: &str = "/setup presets [local-only|local-assisted|hosted] [project|global]\nChoose the main model first with /models. For hosted assistance, explicitly assign a hosted coach in /roles. Every preset is previewed and confirmed; no model is downloaded.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Preset {
    Local,
    Assisted,
    Hosted,
}

impl Preset {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "local-only" => Some(Self::Local),
            "local-assisted" => Some(Self::Assisted),
            "hosted" => Some(Self::Hosted),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Local => "Local only",
            Self::Assisted => "Local with approved hosted assistance",
            Self::Hosted => "Hosted",
        }
    }
}

// A private/LAN address is not proof that inference stays on this machine.
fn local(config: &Config, model: &ModelSelection) -> bool {
    match config.selection_base_url(model) {
        Some(url) => url::Url::parse(&url).ok().is_some_and(|url| {
            url.host_str().is_some_and(|host| {
                host == "localhost"
                    || host
                        .trim_matches(['[', ']'])
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            })
        }),
        None => matches!(model.provider.as_str(), "mlx" | "apple"),
    }
}

fn changes(config: &Config, preset: Preset) -> Result<Vec<Change>> {
    let main = config.selection();
    ensure!(
        local(config, &main) == (preset != Preset::Hosted),
        "{} needs a {} main model. Choose it explicitly with /models first; nothing changed.",
        preset.label(),
        if preset == Preset::Hosted {
            "hosted"
        } else {
            "loopback or on-device"
        }
    );
    let coach = if preset == Preset::Assisted {
        let coach = config.role_selection("coach").context(
            "Assign a hosted coach with /roles first; no model is chosen or downloaded automatically.",
        )?;
        ensure!(
            !local(config, coach) && config.selection_base_url(coach).is_some(),
            "The assisted preset needs an explicitly assigned hosted coach."
        );
        coach
    } else {
        &main
    };
    let mut changes = Vec::new();
    for role in TEXT_ROLES {
        changes.extend(crate::config::selection::changes(
            config,
            if *role == "coach" { coach } else { &main },
            role,
        )?);
    }
    let mut set = |path: &str, value: toml::Value| {
        changes.push(Change::Set {
            path: path.into(),
            value,
        })
    };
    // Mask every inherited chain individually: TOML tables merge across scopes.
    set(
        "retry.fallback_chains",
        toml::Value::try_from(
            config
                .retry
                .fallback_chains
                .keys()
                .map(|key| (key.clone(), Vec::<String>::new()))
                .collect::<std::collections::BTreeMap<_, _>>(),
        )?,
    );
    set("memory.backend", "provenance".into());
    set("memory.enabled", true.into());
    set("laya.enabled", false.into());
    set("assistance.teacher_enabled", false.into());
    set("improve.distill", false.into());
    set("assistance.lean_local", (preset != Preset::Hosted).into());
    set("escalation.enabled", (preset == Preset::Assisted).into());
    set(
        "escalation.allow_remote",
        (preset == Preset::Assisted).into(),
    );
    set("escalation.auto_after_failed_rounds", 0.into());
    set("assistance.local.escalate_after", 0.into());
    if preset == Preset::Assisted {
        set(
            "approval",
            if config.approval == Approval::Readonly {
                "readonly"
            } else {
                "ask"
            }
            .into(),
        );
    }
    Ok(changes)
}

fn validate(
    config: &Config,
    preset: Preset,
    main: &ModelSelection,
    coach: &ModelSelection,
) -> Result<()> {
    ensure!(
        config.selection().provider == main.provider
            && config.model == main.model
            && config.selection_base_url(&config.selection()) == config.selection_base_url(main),
        "An override changes the reviewed main model; use project scope or remove that override."
    );
    ensure!(
        local(config, &config.selection()) == (preset != Preset::Hosted),
        "An override conflicts with the preset's main model route."
    );
    for role in TEXT_ROLES {
        let selected = config
            .role_selection(role)
            .context("A preset role is unassigned")?;
        let expected = if *role == "coach" { coach } else { main };
        ensure!(
            selected.provider == expected.provider
                && selected.model == expected.model
                && config.selection_base_url(selected) == config.selection_base_url(expected),
            "An override conflicts with the reviewed {role} role; use project scope or remove that override."
        );
    }
    ensure!(
        config.retry.fallback_chains.values().all(Vec::is_empty),
        "An inherited fallback chain conflicts with this preset; use project scope."
    );
    ensure!(
        config.memory.enabled
            && config.memory.backend == MemoryBackendId::Provenance
            && !config.laya.enabled
            && !config.assistance.teacher_enabled
            && !config.improve.distill,
        "An auxiliary-model override conflicts with this preset; use project scope."
    );
    ensure!(
        config.escalation.enabled == (preset == Preset::Assisted)
            && config.escalation.allow_remote == (preset == Preset::Assisted)
            && config.escalation.auto_after_failed_rounds == 0
            && config.assistance.local.escalate_after == 0,
        "An escalation override conflicts with this preset; use project scope."
    );
    if preset == Preset::Assisted {
        ensure!(
            matches!(config.approval, Approval::Ask | Approval::Readonly),
            "Hosted assistance requires Ask or Readonly approval; remove the conflicting override."
        );
    }
    Ok(())
}

pub(super) fn command(
    shell: &mut Shell,
    app: &App,
    agent: &mut Agent<'_>,
    args: &[&str],
) -> Result<Step> {
    let preset = match args.first() {
        None => match shell.ask(
            "Setup preset",
            USAGE,
            &[
                ("Local only", 'l'),
                ("Local with approved hosted assistance", 'a'),
                ("Hosted", 'h'),
                ("Cancel", 'c'),
            ],
        ) {
            Some(0) => Preset::Local,
            Some(1) => Preset::Assisted,
            Some(2) => Preset::Hosted,
            _ => return Ok(Step::Done),
        },
        Some(name) => match Preset::parse(name) {
            Some(preset) => preset,
            None => {
                shell.note(TITLE, USAGE);
                return Ok(Step::Done);
            }
        },
    };
    let scope = match args.get(1) {
        Some(&"project") => Scope::Project,
        Some(&"global") => Scope::Global,
        None => match shell.ask(
            "Save setup preset",
            "Project overrides are never silently cleared.",
            &[
                ("This project", 'p'),
                ("All projects (global defaults)", 'g'),
                ("Cancel", 'c'),
            ],
        ) {
            Some(0) => Scope::Project,
            Some(1) => Scope::Global,
            _ => return Ok(Step::Done),
        },
        _ => {
            shell.note(TITLE, USAGE);
            return Ok(Step::Done);
        }
    };
    if args.len() > 2 {
        shell.note(TITLE, USAGE);
        return Ok(Step::Done);
    }
    let config = app.reload()?;
    let changes = changes(&config, preset)?;
    let main = config.selection();
    let coach = if preset == Preset::Assisted {
        config
            .role_selection("coach")
            .context("coach is unassigned")?
            .clone()
    } else {
        main.clone()
    };
    let check = |config: &Config| validate(config, preset, &main, &coach);
    let future = settings_store::preview_config(&app.layout, scope, &changes, &app.overlay)?;
    check(&future)?;
    let memory = crate::llm::local_fit::physical_memory_bytes()
        .map(|bytes| format!("{:.1} GiB physical RAM", bytes as f64 / (1u64 << 30) as f64))
        .unwrap_or_else(|| "Physical RAM unknown".into());
    let roles = TEXT_ROLES
        .iter()
        .map(|role| {
            format!(
                "  {role}: {}",
                future.for_role(role).selection().display_name()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let preview = format!(
        "{} · {scope:?}\nMain (unchanged): {}\n{roles}\n\n{memory}; current main context: {} tokens. Local presets enable the lean tool/context profile. Use `dial models recommend` for weight/KV memory-fit estimates; RAM alone is not a capability or VRAM measurement.\n\nSave these defaults:\n• Clear automatic provider fallback chains.\n• Use local provenance memory (no embedding or memory-model calls); disable Laya and background teacher assistance.\n• Disable automatic escalation.{}\n\nNo provider probes, model selection, downloads, or credential changes. Existing sandbox and edit protections are unchanged. This config preset is not a network firewall: explicit model/profile changes, custom agents and network tools retain their own policies. Global settings remain overridable by other projects.\n\nApply this reviewed setup?",
        preset.label(),
        main.display_name(),
        config.context_window,
        if preset == Preset::Assisted {
            "\n• Enable the reviewed hosted coach and require Ask approval (Readonly remains Readonly). Approved coach questions disclose their supplied evidence to that provider."
        } else {
            "\n• Disable coach escalation."
        }
    );
    shell.note(TITLE, &preview);
    if shell.ask(
        "Apply setup preset",
        &preview,
        &[
            ("Cancel — change nothing", 'c'),
            ("Save reviewed settings", 's'),
        ],
    ) != Some(1)
    {
        return Ok(Step::Done);
    }
    app.save_settings_checked(scope, &changes, check)?;
    let fresh = app.reload()?;
    shell.note(
        TITLE,
        format!(
            "{} saved ({scope:?}). No models were downloaded.",
            preset.label()
        ),
    );
    Ok(shell.config_changed(app, agent, fresh))
}

#[cfg(test)]
mod tests;
