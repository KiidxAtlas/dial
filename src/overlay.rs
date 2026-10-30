use crate::args::Cli;
use anyhow::Result;

pub(crate) fn overlay(cli: &Cli) -> Result<toml::Table> {
    let mut table = toml::Table::new();
    if let Some(model) = &cli.model {
        table.insert("model".into(), model.clone().into());
    }
    if let Some(url) = &cli.endpoint {
        table.insert(
            "provider".into(),
            cli.model_flags
                .provider
                .as_deref()
                .unwrap_or("endpoint")
                .into(),
        );
        let mut endpoint = toml::Table::new();
        endpoint.insert("base_url".into(), url.clone().into());
        if let Some(var) = &cli.api_key_env {
            endpoint.insert("api_key_env".into(), var.clone().into());
        }
        table.insert("endpoint".into(), endpoint.into());
    }
    let approval = if cli.yes {
        Some("auto".to_owned())
    } else if let Some(mode) = &cli.approval_mode {
        if cli.approval.is_some() {
            anyhow::bail!("use either --approval or --approval-mode, not both");
        }
        Some(
            dial::cli::cli_modes::approval_alias(mode)
                .map_err(anyhow::Error::msg)?
                .to_owned(),
        )
    } else {
        cli.approval.clone()
    };
    if let Some(a) = approval {
        table.insert("approval".into(), a.into());
    }
    if let Some(mode) = &cli.sandbox {
        let mut sandbox = toml::Table::new();
        sandbox.insert("mode".into(), mode.clone().into());
        table.insert("sandbox".into(), sandbox.into());
    }
    if let Some(mode) = &cli.edit_mode {
        let mut edit = toml::Table::new();
        edit.insert("mode".into(), mode.clone().into());
        table.insert("edit".into(), edit.into());
    }
    if let Some(profile) = &cli.profile {
        table.insert("model_profile".into(), profile.clone().into());
    }
    if cli.compaction_strategy.is_some()
        || cli.compaction_threshold_tokens.is_some()
        || cli.compaction_reserve_tokens.is_some()
    {
        let mut compaction = toml::Table::new();
        if let Some(strategy) = &cli.compaction_strategy {
            compaction.insert("strategy".into(), strategy.clone().into());
        }
        for (key, input, max) in [
            (
                "threshold_tokens",
                &cli.compaction_threshold_tokens,
                9_007_199_254_740_991u64,
            ),
            (
                "reserve_tokens",
                &cli.compaction_reserve_tokens,
                u64::from(u32::MAX),
            ),
        ] {
            if let Some(input) = input {
                let value = if input == "auto" {
                    toml::Value::String("auto".into())
                } else {
                    let tokens = input.parse::<u64>().map_err(|_| {
                        anyhow::anyhow!("compaction.{key} requires a positive integer or auto")
                    })?;
                    anyhow::ensure!(
                        tokens > 0 && tokens <= max,
                        "compaction.{key} must be in 1..={max} or auto"
                    );
                    toml::Value::Integer(tokens as i64)
                };
                compaction.insert(key.into(), value);
            }
        }
        table.insert("compaction".into(), compaction.into());
    }
    Ok(table)
}

#[cfg(test)]
mod tests;
