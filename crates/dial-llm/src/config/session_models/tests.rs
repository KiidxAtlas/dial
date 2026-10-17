use super::*;

fn table(text: &str) -> toml::Table {
    text.parse().unwrap()
}

#[test]
fn apply_restores_pinned_keys_and_removes_ones_absent_at_capture() {
    let pin = SessionModels::capture(&table(
        "model = 'a'\nprovider = 'mlx'\ntheme = 'old'\n[model_roles.small]\nprovider = 'mlx'\nmodel = 's'\n",
    ));
    let mut merged = table(
        "model = 'b'\nprovider = 'openai'\nmodel_profile = 'fast'\ntheme = 'new'\n[endpoint]\nbase_url = 'https://api.openai.com/v1'\n[model_roles.task]\nprovider = 'openai'\nmodel = 't'\n",
    );
    pin.apply(&mut merged);
    assert_eq!(
        merged,
        table(
            "model = 'a'\nprovider = 'mlx'\ntheme = 'new'\n[model_roles.small]\nprovider = 'mlx'\nmodel = 's'\n"
        )
    );
}

#[test]
fn adopting_own_writes_takes_only_the_written_keys() {
    let mut pin = SessionModels::capture(&table(
        "model = 'a'\ncontext_window = 'auto'\n[model_roles.small]\nprovider = 'mlx'\nmodel = 's'\n",
    ));
    let disk = table(
        "model = 'other-tab'\ncontext_window = 64000\n[model_roles.small]\nprovider = 'mlx'\nmodel = 'other-small'\n[model_roles.task]\nprovider = 'openai'\nmodel = 'mine'\n",
    );
    pin.adopt_paths(&disk, ["context_window", "model_roles.task.model", "theme"]);
    let mut merged = disk.clone();
    pin.apply(&mut merged);
    assert_eq!(
        merged,
        table(
            "model = 'a'\ncontext_window = 64000\n[model_roles.small]\nprovider = 'mlx'\nmodel = 's'\n[model_roles.task]\nprovider = 'openai'\nmodel = 'mine'\n"
        )
    );
}

#[test]
fn adopting_a_role_takes_its_complete_selection() {
    let mut pin = SessionModels::capture(&table(
        "model = 'a'\nmodel_profile = 'old'\n[model_roles.small]\nprovider = 'mlx'\nmodel = 's'\n",
    ));
    let disk = table(
        "model = 'b'\nprovider = 'openai'\n[endpoint]\nbase_url = 'https://api.openai.com/v1'\n",
    );
    pin.adopt_role(&disk, "main");
    pin.adopt_role(&disk, "small");
    let mut merged = table("model = 'c'\n[model_roles.small]\nprovider = 'mlx'\nmodel = 'x'\n");
    pin.apply(&mut merged);
    assert_eq!(merged, disk);
}
