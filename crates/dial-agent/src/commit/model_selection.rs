//! Model override for the commit command (`--model`), applied to the configuration used for generation.

use crate::config::types::Config;

/// Return the configuration generation should use. Without an override this is the configured
/// `provider/model`; the `small` role selection is cleared so the override wins.
pub(crate) fn resolve_commit_config(config: &Config, override_model: Option<&str>) -> Config {
    let Some(spec) = override_model else {
        return config.clone();
    };
    let mut resolved = config.clone();
    match spec.split_once('/') {
        Some((provider, model)) => {
            resolved.provider = Some(provider.to_owned());
            resolved.model = model.to_owned();
        }
        None => resolved.model = spec.to_owned(),
    }
    resolved.model_roles.remove("small");
    resolved
}
