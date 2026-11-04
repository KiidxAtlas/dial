//! Managed Laya service commands.
use crate::args::{Cli, LayaCommand};
use crate::overlay::overlay;
use anyhow::Result;
use dial::config::{paths::Layout, types::Config};

pub(crate) fn run(cli: &Cli, layout: &Layout, action: &LayaCommand) -> Result<()> {
    use dial::agent::laya;
    match action {
        LayaCommand::Install { checkpoint } => {
            eprintln!("Installing Dial-owned Laya runtime and checkpoint…");
            if let Some(model) = laya::install(layout, checkpoint.as_deref())? {
                save_checkpoint(layout, &model)?;
            }
            println!(
                "Dial Laya installed in {}. Run dial laya start, then /laya on inside Dial.",
                dial::config::paths::laya_dir(&layout.user_dir).display()
            );
        }
        LayaCommand::Start | LayaCommand::Status => {
            let config = Config::load(layout, Some(overlay(cli)?))?;
            let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let health = if matches!(action, LayaCommand::Start) {
                laya::ensure_service(&config.laya_service(), cancel, true)?
            } else {
                laya::health(&config.laya_service(), cancel)?
            };
            println!("{}", serde_json::to_string_pretty(&health)?);
        }
    }
    Ok(())
}

/// Persist only service state; installing weights must not change the selected decision engine.
fn save_checkpoint(layout: &Layout, model: &std::path::Path) -> Result<()> {
    use dial::config::settings_store::{self, Change, Scope};
    settings_store::save(
        layout,
        Scope::Global,
        &[Change::Set {
            path: "laya.checkpoint".into(),
            value: model.display().to_string().into(),
        }],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests;
