use super::*;
use serde_json::json;

#[test]
fn actual_notebook_actions_keep_their_approval_contract() {
    let mut registry = Registry::new();
    registry.register(std::sync::Arc::new(
        crate::tools::fs::notebook::Notebook::default(),
    ));
    for action in ["read", "edit", "insert", "delete", "move"] {
        let call = ToolCall {
            id: "notebook-policy".into(),
            name: "notebook".into(),
            arguments: json!({"action":action,"path":"notes.ipynb"}),
            malformed_arguments: false,
        };
        assert_eq!(needs_approval(&registry, &call), action != "read");
        assert!(
            !workspace_write(&call),
            "notebook must retain its explicit mutation approval"
        );
    }
    assert!(!workspace_write(&ToolCall {
        id: String::new(),
        name: "notebook_edit".into(),
        arguments: json!({}),
        malformed_arguments: false
    }));
}

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
fn external_media_reads_are_not_plan_safe_or_parallel_reads() {
    let registry = Registry::new();
    for path in [
        "clip.mp4",
        "clip.MOV:01:23",
        "photo.heic",
        "a.pdf:page=2:img",
    ] {
        for args in [json!({"path":path}), json!({"paths":["a.rs",path]})] {
            assert!(!is_read_only(&registry, "read", &args), "{args}");
            assert!(!parallel_read(&registry, "read", &args), "{args}");
        }
    }
    for path in ["photo.png", "drawing.svg:img", "a.rs:1-20"] {
        assert!(
            is_read_only(&registry, "read", &json!({"path":path})),
            "{path}"
        );
    }
}
#[test]
fn shell_inspection_is_conservative() {
    for text in [
        "pwd",
        "ls 'a b'",
        "cat \"a b\"",
        "find . -name '*.rs'",
        "cat '*.rs'",
        r"cat \*.rs",
        "cat '~'",
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
        "find . -delet?",
        "sed -n 1p [-]i f",
        "tree [-]o out",
        "ls *",
        "cat ~/key.pem",
    ] {
        assert!(!safe_bash(text), "{text}");
    }
}

#[test]
fn the_read_only_shell_rule_matches_the_policy() {
    for program in SAFE_BASH_PROGRAMS {
        let example = match *program {
            "uname" => "uname -a".to_owned(),
            "date" => "date -u".to_owned(),
            "sed" => "sed -n '1,5p' f".to_owned(),
            other => format!("{other} x"),
        };
        assert!(safe_bash(&example), "{example}");
    }
    let rule = safe_bash_rule();
    assert!(rule.contains("wc") && rule.contains("`cwd`"), "{rule}");
    // The forms the rule forbids are the ones the policy refuses.
    for text in [
        "cd crates && wc -l a.rs",
        "cd crates",
        "wc -l a.rs | sort",
        "wc -l a.rs; ls",
        "X=1 wc -l a.rs",
        "rg pattern",
    ] {
        assert!(!safe_bash(text), "{text}");
    }
}

#[test]
fn words_bash_would_expand_never_skip_approval_or_checkpoints() {
    let registry = Registry::new();
    // Bash expands these after the check: with a file named `-delete` in the workspace,
    // `find . -delet?` runs `find . -delete`, and `[-]i` reaches `sed` as `-i`.
    for text in [
        "find . -delet?",
        "find . -delete*",
        "find . -de[l]ete",
        "sed -n 1p [-]i f",
        "tree [-]o out",
        "file [-]C",
        "ls -d */",
        "cat ~/.ssh/config",
        "ls ~",
        "ls a 'b'*",
    ] {
        assert!(shell_words(text).is_none(), "{text}");
        assert!(!safe_bash(text), "{text}");
        assert!(
            !is_read_only(&registry, "bash", &json!({ "command": text })),
            "{text}"
        );
        assert!(opaque_shell_effect(text), "{text}");
        assert!(
            !read_only_inspection_batch(&format!("pwd; {text}")),
            "{text}"
        );
        // The write tier asks too: the unexpanded words cannot show what runs.
        assert!(destructive_bash(text), "{text}");
    }
    // Quoted or escaped, the same characters are literal arguments.
    for text in [
        "find . -name '*.rs'",
        "find . -name \"*.rs\"",
        "find . -name \\*.rs",
        "grep -n 'a[0-9]?' src/main.rs",
        "ls '~' \\~",
    ] {
        assert!(safe_bash(text), "{text}");
    }
    assert_eq!(
        shell_words("find . -name '*.rs' \\?").unwrap(),
        ["find", ".", "-name", "*.rs", "?"]
    );
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
fn monitor_start_requires_fresh_confirmation_even_with_auto_or_reusable_grants() {
    struct Confirm {
        calls: usize,
        decision: Decision,
    }
    impl Ui for Confirm {
        fn approve(&mut self, _: &str, _: &str) -> Decision {
            self.calls += 1;
            self.decision
        }
    }
    for call in [
        file_call(
            "monitor",
            json!({"action":"start", "name":"site", "path":"site.log"}),
        ),
        file_call(
            "bg",
            json!({"action":"watch", "name":"site", "path":"site.log"}),
        ),
    ] {
        for mode in [Approval::Auto, Approval::Write, Approval::Ask] {
            let mut rules = vec![rule(Effect::Allow, "monitor", &[])];
            let mut ui = Confirm {
                calls: 0,
                decision: Decision::AllowAlways,
            };
            for _ in 0..2 {
                assert!(matches!(
                    authorize(mode, &mut rules, Path::new("/p"), &call, "", &mut ui),
                    Ok(false)
                ));
            }
            assert_eq!(ui.calls, 2);
            assert_eq!(
                rules.len(),
                1,
                "watch approval must not create reusable grants"
            );
            ui.decision = Decision::Deny;
            assert!(matches!(
                authorize(mode, &mut rules, Path::new("/p"), &call, "", &mut ui),
                Err(Denial::User)
            ));
        }
        let mut rules = vec![rule(Effect::Deny, "monitor", &[])];
        let mut ui = Confirm {
            calls: 0,
            decision: Decision::Allow,
        };
        assert!(matches!(
            authorize(
                Approval::Auto,
                &mut rules,
                Path::new("/p"),
                &call,
                "",
                &mut ui
            ),
            Err(Denial::Rule)
        ));
        rules.clear();
        assert!(matches!(
            authorize(
                Approval::Readonly,
                &mut rules,
                Path::new("/p"),
                &call,
                "",
                &mut ui
            ),
            Err(Denial::ReadOnly)
        ));
        assert_eq!(ui.calls, 0);
    }
}

#[test]
fn consolidated_permissions_preserve_member_scope() {
    let call = file_call("git", json!({"forge":"github", "op":"pr_view"}));
    let root = Path::new("/p");
    assert!(rule(Effect::Allow, "github", &[]).matches(&call, root));
    assert!(rule(Effect::Deny, "github", &[]).matches(&call, root));
    assert!(rule(Effect::Deny, "git", &[]).matches(&call, root));
    assert!(!rule(Effect::Allow, "git", &[]).matches(&call, root));
    assert!(!rule(Effect::Allow, "glab_mr", &[]).matches(&call, root));
    assert_eq!(approval_grant(&call).rule.unwrap().tool, "github");
}

#[test]
fn consolidated_path_rules_use_the_original_owners_path_contract() {
    let root = Path::new("/p");
    let fetched = file_call(
        "read",
        json!({"fetch":true,"path":"https://example.com/page"}),
    );
    let old = file_call("web_fetch", json!({"url":"https://example.com/page"}));
    for effect in [Effect::Allow, Effect::Deny] {
        let scoped = rule(effect, "web_fetch", &["**"]);
        assert_eq!(scoped.matches(&fetched, root), scoped.matches(&old, root));
    }
    assert!(!rule(Effect::Allow, "web_fetch", &["**"]).matches(&fetched, root));
    let structural = file_call(
        "grep",
        json!({"ast":true,"path":"src/main.rs","pattern":"foo($A)"}),
    );
    let old = file_call(
        "ast_search",
        json!({"path":"src/main.rs","pattern":"foo($A)"}),
    );
    for effect in [Effect::Allow, Effect::Deny] {
        for scope in ["src/**", "docs/**"] {
            let scoped = rule(effect, "ast_search", &[scope]);
            assert_eq!(
                scoped.matches(&structural, root),
                scoped.matches(&old, root)
            );
        }
    }
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
            MutationPolicy {
                rules: &[],
                root: &primary,
                extra_roots: &extra,
                read_only: false,
            },
            &call,
            "write",
            &[attached.join("note.txt")],
            false,
        )
        .is_ok()
    );
    assert!(
        authorize_mutation_paths(
            MutationPolicy {
                rules: &[],
                root: &primary,
                extra_roots: &extra,
                read_only: false,
            },
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
            // The prompt itself says approval is required; no policy line is appended.
            assert!(!summary.contains("Policy:"));
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

#[test]
fn finite_batch_surface_never_converts_persistent_choices_into_grants() {
    struct Prompt(Decision, usize);
    impl Ui for Prompt {
        fn approve(&mut self, _: &str, _: &str) -> Decision {
            self.1 += 1;
            self.0
        }
    }
    let calls = vec![bash("first"), bash("second")];
    let cancel = std::sync::atomic::AtomicBool::new(false);
    for (decision, allowed) in [
        (Decision::Allow, true),
        (Decision::AllowAlways, false),
        (Decision::AllowProject, false),
        (Decision::Deny, false),
    ] {
        let mut ui = Prompt(decision, 0);
        assert_eq!(ui.approve_batch(&calls, &cancel), vec![allowed; 2]);
        assert_eq!(
            ui.1, 1,
            "one real approval surface call for the finite batch"
        );
    }
    cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    let mut ui = Prompt(Decision::Allow, 0);
    assert_eq!(ui.approve_batch(&calls, &cancel), vec![false; 2]);
    assert_eq!(ui.1, 0, "cancelled batch must not prompt");
}

#[test]
fn batch_rows_summarize_targets_and_sizes_without_raw_contents() {
    let call = |name: &str, arguments: serde_json::Value| ToolCall {
        id: "call-7f3a".into(),
        name: name.into(),
        arguments,
        malformed_arguments: false,
    };
    let body = "line\n".repeat(400);
    let cases = [
        (
            call(
                "write",
                serde_json::json!({"path": "src/big.rs", "content": body}),
            ),
            "write src/big.rs · 400 lines, 2.0 KiB",
        ),
        (
            call(
                "edit",
                serde_json::json!({"input": "[src/a.rs#3FA9]\nPUT 4.=4:\n+    let x = 1;\n+    let y = 2;"}),
            ),
            "edit src/a.rs · 2 new lines",
        ),
        (
            call(
                "ast_edit",
                serde_json::json!({"path": "src/c.rs", "pattern": "x"}),
            ),
            "ast_edit src/c.rs",
        ),
    ];
    for (call, expected) in cases {
        let line = batch_call_line(&call);
        assert_eq!(line, expected);
        assert!(!line.contains("call-7f3a") && !line.contains("let x"));
    }
}

#[test]
fn plan_mode_denial_names_plan_mode_for_the_model() {
    assert!(Denial::ReadOnly.message_in(true).contains("plan mode"));
    assert!(Denial::ReadOnly.message_in(true).starts_with("denied"));
    assert_eq!(
        Denial::ReadOnly.message_in(false),
        Denial::ReadOnly.message()
    );
    assert_eq!(Denial::User.message_in(true), Denial::User.message());
}

#[test]
fn read_only_inspection_batches_split_only_at_unquoted_separators() {
    let git = "git -c core.fsmonitor=false -c core.pager=cat --no-pager";
    let mixed_git = format!(
        "{git} status --short --untracked-files=all; {git} diff --no-color --no-ext-diff --no-textconv --stat"
    );
    for command in [
        r#"ls -la "a b"; ls -d '*/'"#,
        "ls -la\nls -d '*/'",
        "ls && pwd",
        r#"ls "a;rm x""#,
        r"ls \;",
        mixed_git.as_str(),
    ] {
        assert!(read_only_inspection_batch(command), "{command}");
    }
    for command in [
        "ls -d */",
        "ls; find . -delet?",
        "ls; cat ~/key.pem",
        "ls > out.txt",
        "ls | wc -l",
        "ls &",
        "ls && rm x",
        "ls; rm -rf target",
        "ls\nrm x",
        "ls;;ls",
        "ls;",
        "ls $HOME; ls",
        "ls `pwd`; ls",
        "ls $(pwd); ls",
        "(ls; ls)",
        "for f in *; do ls \"$f\"; done",
        "ls; sed -i s/a/b/ f",
        "ls; sed -n 'w x' f",
        "cd crates && ls",
        "sh -c 'ls'",
        "env ls",
        "x=1; ls",
        "ls \"unterminated",
        "ls \\\n",
        "ls \\\n; rm x",
    ] {
        assert!(!read_only_inspection_batch(command), "{command}");
    }
}

#[test]
fn opaque_shell_effect_counts_only_unverified_batches() {
    for (command, opaque) in [
        (r#"ls -la "a b"; ls -d '*/'"#, false),
        ("ls -la\nls -d '*/'", false),
        ("ls -la; ls -d */", true),
        ("ls && pwd", false),
        ("ls > out.txt", true),
        ("ls; rm -rf target", true),
        ("ls; sed -i s/a/b/ f", true),
        ("for f in *; do ls \"$f\"; done", true),
        ("sh -c 'ls'", true),
        ("ls \\\n; rm x", true),
    ] {
        assert_eq!(opaque_shell_effect(command), opaque, "{command}");
    }
}

#[test]
fn shell_consumers_share_lexical_boundaries_without_sharing_authority() {
    for command in [r#"echo 'a|b;&&c'"#, r#"echo "a|b;&&c""#, r"echo a\|b\;c\&d"] {
        assert!(shell_words(command).is_some(), "{command:?}");
        assert_eq!(pipe_stages(command), Some(vec![command]), "{command:?}");
        assert_eq!(
            top_level_segments(command),
            Some(vec![command]),
            "{command:?}"
        );
        assert!(safe_bash(command), "{command:?}");
        assert!(read_only_inspection_batch(command), "{command:?}");
    }
    for command in [
        "echo 'open",
        "echo \"open",
        "echo trailing\\",
        "echo a\\\nb",
        "echo \"a\\\nb\"",
    ] {
        assert!(shell_words(command).is_none(), "{command:?}");
        assert!(pipe_stages(command).is_none(), "{command:?}");
        assert!(top_level_segments(command).is_none(), "{command:?}");
        assert!(!read_only_inspection_batch(command), "{command:?}");
    }
    for (command, complete) in [
        ("sh -c \"sh -c 'rm -f x'\"", true),
        ("env -S 'sh -c \"rm -f x\"'", true),
        ("sh -c 'echo a\\;b'", true),
        ("sh -c 'cat \"*.rs\"'", true),
        ("sh -c 'cat *.rs'", false),
        ("env sh -c 'cat ~/key.pem'", false),
        ("sh -c 'ls; rm x'", false),
        ("sh -c 'echo trailing\\'", false),
    ] {
        assert_eq!(shell_argvs(command).1, complete, "{command:?}");
        assert!(
            !safe_bash(command),
            "a wrapper cannot grant inspection authority: {command:?}"
        );
    }
    let status = "git -c core.fsmonitor=false -c core.pager=cat --no-pager status --short --untracked-files=all";
    let quoted = status.replace("status", "'status'");
    for separator in [";\n", " &&\n", "\n"] {
        let batch = format!("{quoted}{separator}{status}");
        assert!(safe_review_batch(&batch), "{batch:?}");
        assert!(
            !safe_bash(&batch),
            "review effects never grant batch approval: {batch:?}"
        );
    }
    for suffix in [";", " &&", ";;", "\\", "\\\n"] {
        assert!(
            !safe_review_batch(&format!("{status}{suffix}")),
            "{suffix:?}"
        );
    }
}
