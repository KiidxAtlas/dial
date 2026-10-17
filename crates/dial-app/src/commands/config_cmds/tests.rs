use super::*;

#[test]
fn preset_names_follow_omp_rule() {
    assert!(valid_name("fast-1_x"));
    assert!(!valid_name("1abc"));
    assert!(!valid_name("a.b"));
    assert!(!valid_name(""));
}

#[test]
fn ssh_add_parses_flags_and_scope() {
    let (name, host, scope) =
        parse_ssh_add("box --host 10.0.0.2 --user dev --port 2222 --key ~/.ssh/id --scope project")
            .unwrap();
    assert_eq!(name, "box");
    assert_eq!(host.host, "10.0.0.2");
    assert_eq!(host.user.as_deref(), Some("dev"));
    assert_eq!(host.port, Some(2222));
    assert_eq!(host.key.as_deref(), Some("~/.ssh/id"));
    assert_eq!(scope, Scope::Project);
}

#[test]
fn ssh_add_rejects_missing_host_and_bad_port() {
    assert!(parse_ssh_add("box").is_err());
    assert!(parse_ssh_add("box --host h --port 0").is_err());
    assert!(parse_ssh_add("box --host h --port 70000").is_err());
    assert!(parse_ssh_add("box --host").is_err());
}

#[test]
fn ssh_remove_parses_scope() {
    assert_eq!(
        parse_ssh_remove("box").unwrap(),
        ("box".into(), Scope::Global)
    );
    assert_eq!(
        parse_ssh_remove("box --scope project").unwrap(),
        ("box".into(), Scope::Project)
    );
    assert!(parse_ssh_remove("box --scope nope").is_err());
}

fn written(changes: &[Change]) -> Vec<String> {
    changes
        .iter()
        .map(|change| match change {
            Change::Set { path, .. } | Change::Remove { path } => path.clone(),
        })
        .collect()
}

#[test]
fn slow_writes_only_slow_mode_and_leaves_fast_alone() {
    let mut config = Config {
        fast_mode: true,
        ..Default::default()
    };
    let plan = slow_plan(&config, "openai", "on").unwrap();
    assert_eq!(written(&plan.changes), vec!["slow_mode"]);
    config.slow_mode = true;
    let plan = slow_plan(&config, "openai", "off").unwrap();
    assert_eq!(written(&plan.changes), vec!["slow_mode"]);
}

#[test]
fn fast_writes_only_fast_state_and_leaves_slow_alone() {
    let config = Config {
        slow_mode: true,
        ..Default::default()
    };
    let plan = fast_plan(&config, "openai", "on").unwrap();
    assert_eq!(written(&plan.changes), vec!["fast_mode"]);
    let plan = fast_plan(&config, "openai", "off").unwrap();
    assert!(written(&plan.changes).is_empty());
}

#[test]
fn fast_off_clears_explicit_fast_tier_only() {
    let config = Config {
        service_tier: "ultrafast".into(),
        ..Default::default()
    };
    let plan = fast_plan(&config, "openai", "off").unwrap();
    assert_eq!(written(&plan.changes), vec!["service_tier"]);
}
