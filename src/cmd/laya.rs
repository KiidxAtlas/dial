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
                use dial::config::settings_store::{self, Change, Scope};
                settings_store::save(
                    layout,
                    Scope::Global,
                    &[
                        Change::Set {
                            path: "model_roles.decision.provider".into(),
                            value: dial::config::laya::PROVIDER.into(),
                        },
                        Change::Set {
                            path: "model_roles.decision.model".into(),
                            value: model.display().to_string().into(),
                        },
                    ],
                )?;
            }
            println!(
                "Dial Laya installed in {}. Run dial laya start, then /laya on inside Dial.",
                layout.user_dir.join("laya").display()
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
