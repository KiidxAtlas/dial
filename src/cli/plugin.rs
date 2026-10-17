//! `dial plugin` (alias `plugins`): lifecycle management for installed plugins and their
//! marketplaces. Plugin state lives under the user directory (`<user_dir>/plugins` and
//! `<user_dir>/marketplaces`), shared across projects.
use crate::storage::paths::Layout;
use crate::tools::plugin::{
    installer::{InstalledPlugin, PluginStore, PluginUpdate},
    marketplace::{MarketplaceEntry, MarketplaceInfo, Marketplaces},
};
use anyhow::Result;
use clap::{Args, Subcommand};
use std::path::PathBuf;

/// `dial plugin [command]`. With no command the installed plugins are listed.
#[derive(Args, Debug, Clone)]
pub struct PluginArgs {
    #[command(subcommand)]
    pub command: Option<PluginCmd>,
}

#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum PluginCmd {
    /// Install a plugin from a local directory, `git+<url>[#ref]`, or `npm:<name>[@version]`.
    Install { source: String },
    /// List installed plugins.
    List {
        /// Print the list as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Remove an installed plugin.
    #[command(alias = "uninstall")]
    Remove { name: String },
    /// Enable an installed plugin.
    Enable { name: String },
    /// Disable an installed plugin without removing it.
    Disable { name: String },
    /// Update one plugin, or every installed plugin when no name is given.
    Update { name: Option<String> },
    /// Report plugins that have newer versions available.
    CheckUpdates {
        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Manage plugin marketplaces.
    Marketplace {
        #[command(subcommand)]
        command: Option<MarketplaceCmd>,
    },
}

#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum MarketplaceCmd {
    /// Register a marketplace by name and index (an https URL or a local JSON path).
    Add { name: String, index: String },
    /// Unregister a marketplace.
    #[command(alias = "rm")]
    Remove { name: String },
    /// Turn a registered marketplace on; its plugins and skills are offered again.
    Enable { name: String },
    /// Turn a registered marketplace off without removing it.
    Disable { name: String },
    /// List registered marketplaces.
    List,
    /// Re-fetch one marketplace index, or all of them when no name is given.
    Refresh { name: Option<String> },
    /// Show the plugin entries offered by one marketplace, or by all of them.
    Entries {
        name: Option<String>,
        /// Print the entries as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Install a plugin offered by a marketplace (`plugin@marketplace`, or a bare unique name).
    Install { plugin: String },
    /// Upgrade marketplace-installed plugins, optionally only the named plugin.
    Upgrade { name: Option<String> },
}

/// Runs the command. Errors are printed with `{:#}`; returns 0 on success and 1 on error.
pub fn run(args: PluginArgs, layout: &Layout) -> Result<i32> {
    match execute(args.command, layout) {
        Ok(()) => Ok(0),
        Err(error) => {
            eprintln!("{error:#}");
            Ok(1)
        }
    }
}

fn execute(command: Option<PluginCmd>, layout: &Layout) -> Result<()> {
    let plugins_root = layout.user_dir.join("plugins");
    let marketplaces_root = layout.user_dir.join("marketplaces");
    match command.unwrap_or(PluginCmd::List { json: false }) {
        PluginCmd::Marketplace { command } => marketplace(
            command.unwrap_or(MarketplaceCmd::List),
            plugins_root,
            marketplaces_root,
        ),
        other => plugin(other, plugins_root),
    }
}

fn plugin(command: PluginCmd, root: PathBuf) -> Result<()> {
    let store = PluginStore::new(root);
    match command {
        PluginCmd::Install { source } => {
            let source = if ["git+", "npm:", "file:"]
                .iter()
                .any(|p| source.starts_with(p))
            {
                source
            } else {
                format!("file:{source}")
            };
            let installed = store.install(&source)?;
            println!("Installed {}", describe_installed(&installed));
        }
        PluginCmd::List { json } => print_plugins(&store.list()?, json)?,
        PluginCmd::Remove { name } => {
            store.remove(&name)?;
            println!("Removed {name}");
        }
        PluginCmd::Enable { name } => {
            store.set_enabled(&name, true)?;
            println!("Enabled {name}");
        }
        PluginCmd::Disable { name } => {
            store.set_enabled(&name, false)?;
            println!("Disabled {name}");
        }
        PluginCmd::Update { name } => print_updates(&store.update(name.as_deref())?),
        PluginCmd::CheckUpdates { json } => {
            let updates = store.check_updates()?;
            if json {
                print_json(&updates)?;
            } else {
                print_updates(&updates);
            }
        }
        PluginCmd::Marketplace { .. } => unreachable!("marketplace is routed by execute"),
    }
    Ok(())
}

fn marketplace(command: MarketplaceCmd, plugins_root: PathBuf, root: PathBuf) -> Result<()> {
    let marketplaces = Marketplaces::new(root);
    match command {
        MarketplaceCmd::Add { name, index } => {
            marketplaces.add(&name, &index)?;
            println!("Added marketplace {name}");
        }
        MarketplaceCmd::Remove { name } => {
            marketplaces.remove(&name)?;
            println!("Removed marketplace {name}");
        }
        MarketplaceCmd::Enable { name } => {
            marketplaces.set_enabled(&name, true)?;
            println!("Enabled marketplace {name}");
        }
        MarketplaceCmd::Disable { name } => {
            marketplaces.set_enabled(&name, false)?;
            println!("Disabled marketplace {name}");
        }
        MarketplaceCmd::List => print_marketplaces(&marketplaces.list()?),
        MarketplaceCmd::Refresh { name } => {
            marketplaces.refresh(name.as_deref())?;
            println!(
                "Refreshed {}",
                name.as_deref().unwrap_or("all marketplaces")
            );
        }
        MarketplaceCmd::Entries { name, json } => {
            let entries = marketplaces.entries(name.as_deref())?;
            if json {
                print_json(&entries)?;
            } else {
                print_entries(&entries);
            }
        }
        MarketplaceCmd::Install { plugin } => {
            let store = PluginStore::new(plugins_root);
            let installed = marketplaces.install(&plugin, &store)?;
            println!("Installed {}", describe_installed(&installed));
        }
        MarketplaceCmd::Upgrade { name } => {
            let store = PluginStore::new(plugins_root);
            print_updates(&marketplaces.upgrade(&store, name.as_deref())?);
        }
    }
    Ok(())
}

fn describe_installed(plugin: &InstalledPlugin) -> String {
    match &plugin.version {
        Some(version) => format!("{}@{version}", plugin.name),
        None => plugin.name.clone(),
    }
}

fn print_plugins(plugins: &[InstalledPlugin], json: bool) -> Result<()> {
    if json {
        return print_json(plugins);
    }
    if plugins.is_empty() {
        println!("No plugins installed");
        return Ok(());
    }
    for plugin in plugins {
        let state = if plugin.enabled {
            "enabled"
        } else {
            "disabled"
        };
        println!(
            "{}\t{}\t{state}\t{}",
            describe_installed(plugin),
            plugin.source,
            plugin.path.display()
        );
    }
    Ok(())
}

fn print_updates(updates: &[PluginUpdate]) {
    if updates.is_empty() {
        println!("No plugins");
        return;
    }
    for update in updates {
        let from = update.from.as_deref().unwrap_or("-");
        let to = update.to.as_deref().unwrap_or("-");
        if update.changed {
            println!("{}: {from} -> {to}", update.name);
        } else {
            println!("{}: up to date ({to})", update.name);
        }
    }
}

fn print_marketplaces(marketplaces: &[MarketplaceInfo]) {
    if marketplaces.is_empty() {
        println!("No marketplaces configured");
        return;
    }
    for marketplace in marketplaces {
        let state = if marketplace.enabled {
            "enabled"
        } else {
            "disabled"
        };
        let index = if marketplace.builtin {
            "built-in"
        } else {
            marketplace.index.as_str()
        };
        println!("{}\t{state}\t{index}", marketplace.name);
    }
}

fn print_entries(entries: &[MarketplaceEntry]) {
    if entries.is_empty() {
        println!("No plugins available");
        return;
    }
    for entry in entries {
        let version = entry
            .version
            .as_deref()
            .map(|v| format!("@{v}"))
            .unwrap_or_default();
        println!("{}@{}{version}", entry.name, entry.marketplace);
        if let Some(description) = &entry.description {
            println!("    {description}");
        }
    }
}

fn print_json<T: serde::Serialize + ?Sized>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

#[cfg(test)]
mod tests;
