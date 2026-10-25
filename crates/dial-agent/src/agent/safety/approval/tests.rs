use super::*;
use serde_json::json;

#[test]
fn ssh_url_file_reads_require_approval() {
    let registry = Registry::new();
    assert!(!is_read_only(
        &registry,
        "read",
        &json!({"path":"ssh://host/etc/hosts"})
    ));
    assert!(!is_read_only(
        &registry,
        "grep",
        &json!({"path":"ssh://host/etc/hosts","pattern":"x"})
    ));
    assert!(is_read_only(
        &registry,
        "read",
        &json!({"path":"src/main.rs"})
    ));
}
#[test]
fn shell_inspection_is_conservative() {
    for text in [
        "pwd",
        "ls 'a b'",
        "cat \"a b\"",
        "find . -name '*.rs'",
        "sed -n '1,10p' file",
        "git -c core.fsmonitor=false -c core.pager=cat --no-pager status --short --untracked-files=all",
        "git -c core.fsmonitor=false -c core.pager=cat --no-pager diff --no-color --no-ext-diff --no-textconv",
        "git -c core.fsmonitor=false -c core.pager=cat --no-pager diff --no-color --no-ext-diff --no-textconv --staged",
    ] {
        assert!(safe_bash(text), "{text}");
    }
    for text in [
        "git status",
        "git diff",
        "git log",
        "git show",
        "rg pattern",
        "ls; rm -rf ~",
        "cat x > y",
        "git diff --output=f",
        "find . -exec rm {} +",
        "echo $(rm x)",
        "ls | sh",
        "PATH=x ls",
        "sudo ls",
        "cargo test",
        "cargo build",
        "npm test",
        "python -m pytest",
        "make -n",
        "rg --pre=sh x",
        "sed -n '1e touch x' f",
    ] {
        assert!(!safe_bash(text), "{text}");
    }
}

#[test]
fn review_batches_do_not_rearm_completion_but_still_require_approval() {
    let status = "git -c core.fsmonitor=false -c core.pager=cat --no-pager status --short --untracked-files=all";
    let diff = "git -c core.fsmonitor=false -c core.pager=cat --no-pager diff --no-color --no-ext-diff --no-textconv";
    for separator in ["; ", " && ", ";\n"] {
        let batch = format!("{status}{separator}{diff}; {diff} --staged");
        assert!(safe_review_batch(&batch), "{batch}");
        assert!(
            !safe_bash(&batch),
            "approval policy must not silently broaden"
        );
    }
    for suffix in [
        "; rm x",
        " && cargo test",
        " | sh",
        " > output",
        " --output=x",
        " -c core.fsmonitor=evil",
        ";",
        "; git diff",
        "$(touch x)",
    ] {
        assert!(
            !safe_review_batch(&format!("{status}; {diff}{suffix}")),
            "{suffix}"
        );
    }
    assert!(!safe_review_batch(""));
    assert!(!safe_review_batch("git status; git diff"));
}

#[test]
fn review_stat_batches_are_read_only_without_broadening_shell_approval() {
    let git = "git -c core.fsmonitor=false -c core.pager=cat --no-pager";
    let diff = format!("{git} diff --no-color --no-ext-diff --no-textconv");
    for flags in ["--stat", "--staged --stat", "--stat --staged"] {
        assert!(safe_bash(&format!("{diff} {flags}")));
    }
    for separator in ["; ", " && ", "\n"] {
        let batch = format!(
            "{git} status --short --untracked-files=all{separator}{diff} --stat{separator}{diff} --staged --stat"
        );
        assert!(safe_review_batch(&batch), "{batch}");
        assert!(!safe_bash(&batch));
        for suffix in [" --output=x", "\nrm x", " | sh", " > x", " --ext-diff"] {
            assert!(!safe_review_batch(&format!("{batch}{suffix}")), "{suffix}");
        }
    }
}

fn bash(command: &str) -> ToolCall {
    ToolCall {
        id: "x".into(),
        name: "bash".into(),
        arguments: serde_json::json!({"command":command}),
        malformed_arguments: false,
    }
}

#[test]
fn always_allow_is_scoped_and_deny_wins_even_in_auto_mode() {
    struct Always(usize);
    impl Ui for Always {
        fn approve(&mut self, _: &str, _: &str) -> Decision {
            self.0 += 1;
            Decision::AllowAlways
        }
    }
    let mut rules = Vec::new();
    let mut ui = Always(0);
    assert!(
        authorize(
            Approval::Ask,
            &mut rules,
            Path::new("/p"),
            &bash("cargo test --lib"),
            "",
            &mut ui
        )
        .is_ok()
    );
    assert!(
        authorize(
            Approval::Ask,
            &mut rules,
            Path::new("/p"),
            &bash("cargo test --test agent"),
            "",
            &mut ui
        )
        .is_ok()
    );
    assert_eq!(ui.0, 1);
    assert!(
        authorize(
            Approval::Ask,
            &mut rules,
            Path::new("/p"),
            &bash("cargo build"),
            "",
            &mut ui
        )
        .is_ok()
    );
    assert_eq!(ui.0, 2);
    assert!(
        authorize(
            Approval::Ask,
            &mut rules,
            Path::new("/p"),
            &bash("cargo test; rm x"),
            "",
            &mut ui
        )
        .is_ok()
    );
    assert_eq!(ui.0, 3, "compound command cannot inherit a prefix grant");
    assert_eq!(
        rules.len(),
        2,
        "compound command cannot create a blanket grant"
    );
    rules.push(PermissionRule {
        effect: Effect::Deny,
        tool: "bash".into(),
        command_prefix: vec!["cargo".into(), "test".into()],
        paths: vec![],
    });
    assert!(matches!(
        authorize(
            Approval::Auto,
            &mut rules,
            Path::new("/p"),
            &bash("cargo test"),
            "",
            &mut ui
        ),
        Err(Denial::Rule)
    ));
    assert!(matches!(
        authorize(
            Approval::Readonly,
            &mut rules,
            Path::new("/p"),
            &bash("cargo build"),
            "",
            &mut ui
        ),
        Err(Denial::ReadOnly)
    ));
}

#[test]
fn project_rules_round_trip_and_require_scoped_shell_grants() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("permissions.toml");
    let rules = vec![PermissionRule {
        effect: Effect::Deny,
        tool: "bash".into(),
        command_prefix: vec!["git".into(), "push".into()],
        paths: vec![],
    }];
    save(&path, &rules).unwrap();
    assert_eq!(load(&path).unwrap(), rules);
    assert!(
        PermissionRule {
            effect: Effect::Allow,
            tool: "bash".into(),
            command_prefix: vec![],
            paths: vec![]
        }
        .validate()
        .is_err()
    );
}

fn file_call(tool: &str, args: Value) -> ToolCall {
    ToolCall {
        id: "x".into(),
        name: tool.into(),
        arguments: args,
        malformed_arguments: false,
    }
}

fn rule(effect: Effect, tool: &str, paths: &[&str]) -> PermissionRule {
    PermissionRule {
        effect,
        tool: tool.into(),
        command_prefix: vec![],
        paths: paths.iter().map(|p| (*p).to_owned()).collect(),
    }
}

#[test]
fn path_rules_match_normalised_project_paths() {
    let root = Path::new("/p");
    let deny = [rule(
        Effect::Deny,
        "write",
        &[".env", "*.pem", "secrets/**"],
    )];
    let blocked = |args: Value| denied_by_rule(&deny, &file_call("write", args), root);
    for path in [
        ".env",
        "./.env",
        "/p/.env",
        "a/../.env",
        "sub/dir/.env",
        "key.pem",
        "secrets/a/b.txt",
        "src/../secrets/x",
    ] {
        assert!(blocked(json!({"path":path,"content":""})), "{path}");
    }
    for path in ["src/main.rs", "env", "secretsx/a"] {
        assert!(!blocked(json!({"path":path,"content":""})), "{path}");
    }
    if cfg!(target_os = "macos") {
        assert!(blocked(json!({"path":".ENV"})));
    }
    assert!(
        !denied_by_rule(&deny, &file_call("edit", json!({"path":".env"})), root),
        "rule names another tool"
    );
}

#[test]
fn conflict_reads_are_judged_by_the_file_they_name() {
    let root = Path::new("/p");
    let deny = [rule(Effect::Deny, "read", &["secrets/**"])];
    let blocked =
        |path: &str| denied_by_rule(&deny, &file_call("read", json!({"path":path})), root);
    assert!(blocked("conflict://secrets/key.txt"));
    assert!(!blocked("conflict://src/main.rs"));
}

#[test]
fn path_deny_fails_closed_and_allow_needs_every_file_covered() {
    let root = Path::new("/p");
    let deny = [rule(Effect::Deny, "edit", &["secrets/**"])];
    let hashline = file_call(
        "edit",
        json!({"input":"*** Begin Patch\n[src/a.rs#AB12]\nPUT 1.=1:\n+x\n[secrets/k#CD34]\nPUT 1.=1:\n+y\n*** End Patch\n"}),
    );
    assert!(
        denied_by_rule(&deny, &hashline, root),
        "hashline patch naming a protected file"
    );
    assert!(
        denied_by_rule(
            &deny,
            &file_call("edit", json!({"input":"nothing parseable"})),
            root
        ),
        "unknown target is denied"
    );
    let allow = rule(Effect::Allow, "edit", &["src/**"]);
    assert!(allow.matches(
        &file_call(
            "edit",
            json!({"path":"src/a/b.rs","old_string":"before","new_string":"after"})
        ),
        root
    ));
    assert!(!allow.matches(
        &file_call(
            "edit",
            json!({"path":"Cargo.toml","old_string":"before","new_string":"after"})
        ),
        root
    ));
    assert!(
        !allow.matches(&hashline, root),
        "allow requires all files covered"
    );
    assert!(
        !allow.matches(&file_call("edit", json!({"input":"x"})), root),
        "unknown target is never allowed"
    );
    assert!(rule(Effect::Allow, "bash", &["src/**"]).validate().is_err());
    assert!(rule(Effect::Allow, "edit", &["["]).validate().is_err());
}

#[test]
fn path_rules_round_trip_through_toml() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("permissions.toml");
    let rules = vec![rule(Effect::Deny, "write", &["*.pem"])];
    save(&path, &rules).unwrap();
    assert_eq!(load(&path).unwrap(), rules);
}

#[test]
fn mutation_paths_accept_attached_roots_and_refuse_others() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let primary = base.join("primary");
    let attached = base.join("attached");
    let outside = base.join("outside");
    for path in [&primary, &attached, &outside] {
        fs::create_dir_all(path).unwrap();
    }
    let call = ToolCall {
        id: "call".into(),
        name: "write".into(),
        arguments: json!({"path": "note.txt"}),
        malformed_arguments: false,
    };
    let extra = [attached.clone()];
    assert!(
        authorize_mutation_paths(
            &[],
            &primary,
            &extra,
            false,
            &call,
            "write",
            &[attached.join("note.txt")],
            false,
        )
        .is_ok()
    );
    assert!(
        authorize_mutation_paths(
            &[],
            &primary,
            &extra,
            false,
            &call,
            "write",
            &[outside.join("note.txt")],
            false,
        )
        .is_err()
    );
}

#[test]
fn policy_explanations_follow_winning_precedence_without_grants() {
    #[derive(Default)]
    struct Observe(Vec<PolicyExplanation>);
    impl Ui for Observe {
        fn policy_explanation(&mut self, _: &str, _: &str, explanation: &PolicyExplanation) {
            self.0.push(explanation.clone());
        }
    }
    let call = bash("cargo test");
    let root = Path::new("/p");
    let rule = |effect| PermissionRule {
        effect,
        tool: "bash".into(),
        command_prefix: vec!["cargo".into()],
        paths: vec![],
    };
    for mode in [
        Approval::Ask,
        Approval::Write,
        Approval::Auto,
        Approval::Readonly,
    ] {
        for reverse in [false, true] {
            let mut rules = vec![rule(Effect::Allow), rule(Effect::Deny)];
            if reverse {
                rules.reverse();
            }
            let mut ui = Observe::default();
            assert!(matches!(
                authorize(mode, &mut rules, root, &call, "", &mut ui),
                Err(Denial::Rule)
            ));
            assert_eq!(ui.0.len(), 1);
            assert_eq!(ui.0[0].reason, PolicyReason::DenyRule);
            assert_eq!(ui.0[0].rule_scope, Some(rule(Effect::Deny).scope()));
            assert_eq!(rules.len(), 2);
        }
    }
    for (mode, reason, allowed) in [
        (Approval::Auto, PolicyReason::AutomaticMode, true),
        (Approval::Readonly, PolicyReason::ReadOnly, false),
        (Approval::Write, PolicyReason::WorkspaceWrite, true),
        (Approval::Ask, PolicyReason::OperatorDeny, false),
    ] {
        let mut ui = Observe::default();
        let mut rules = vec![];
        assert_eq!(
            authorize(mode, &mut rules, root, &call, "", &mut ui).is_ok(),
            allowed
        );
        assert_eq!(ui.0.last().unwrap().reason, reason);
        assert!(rules.is_empty());
    }
    let mut ui = Observe::default();
    let mut rules = vec![rule(Effect::Deny)];
    let cancel = std::sync::atomic::AtomicBool::new(true);
    assert!(matches!(
        authorize_with_cancel(
            Approval::Auto,
            &mut rules,
            root,
            &call,
            "",
            &mut ui,
            &cancel
        ),
        Err(Denial::User)
    ));
    assert_eq!(ui.0[0].reason, PolicyReason::Cancelled);
    let encoded = serde_json::to_value(&ui.0[0]).unwrap();
    assert_eq!(encoded["reason"], "cancelled");
    assert!(encoded.get("rule_scope").is_none());
}

#[test]
fn policy_prompt_and_cancelled_reusable_response_do_not_create_grants() {
    struct Cancel<'a>(&'a std::sync::atomic::AtomicBool, Vec<PolicyReason>);
    impl Ui for Cancel<'_> {
        fn policy_explanation(&mut self, _: &str, _: &str, explanation: &PolicyExplanation) {
            self.1.push(explanation.reason);
        }
        fn approve(&mut self, _: &str, summary: &str) -> Decision {
            assert!(summary.contains("Policy: operator approval required"));
            self.0.store(true, std::sync::atomic::Ordering::Release);
            Decision::AllowAlways
        }
    }
    let flag = std::sync::atomic::AtomicBool::new(false);
    let mut ui = Cancel(&flag, vec![]);
    let mut rules = vec![];
    assert!(
        authorize_with_cancel(
            Approval::Ask,
            &mut rules,
            Path::new("/p"),
            &bash("cargo test"),
            "",
            &mut ui,
            &flag
        )
        .is_err()
    );
    assert!(rules.is_empty());
    assert_eq!(
        ui.1,
        [PolicyReason::PromptRequired, PolicyReason::Cancelled]
    );
}
