use super::*;

const TOOLS: &[&str] = &["read", "grep", "glob", "edit", "write", "bash", "bg"];

fn on() -> BashInterceptorConfig {
    BashInterceptorConfig {
        enabled: true,
        mode: InterceptMode::Block,
        patterns: None,
    }
}

fn hint_mode() -> BashInterceptorConfig {
    BashInterceptorConfig::default()
}

fn blocked(config: &BashInterceptorConfig, command: &str) -> Option<String> {
    match check(config, TOOLS, command)? {
        Verdict::Block(message) => Some(message),
        Verdict::Hint(line) => panic!("expected a block, got {line}"),
    }
}

/// Tool of the default rule whose hint `command` gets.
fn hinted_tool(command: &str) -> Option<String> {
    let Verdict::Hint(line) = check(&hint_mode(), TOOLS, command)? else {
        panic!("expected a hint for {command}");
    };
    default_rules()
        .into_iter()
        .find(|r| line == format!("[hint] {}", r.message))
        .map(|r| r.tool)
}

/// Tool of the default rule that blocked `command`, exactly matched by message.
fn blocked_tool_with(
    config: &BashInterceptorConfig,
    tools: &[&str],
    command: &str,
) -> Option<String> {
    let Verdict::Block(message) = check(config, tools, command)? else {
        panic!("expected a block for {command}");
    };
    default_rules()
        .into_iter()
        .find(|r| message == format!("Blocked: {}\n\nOriginal command: {command}", r.message))
        .map(|r| r.tool)
}

fn blocked_tool(command: &str) -> Option<String> {
    blocked_tool_with(&on(), TOOLS, command)
}

fn segments(command: &str) -> Vec<(String, bool)> {
    extract_flat_shell_command_segments(command)
        .into_iter()
        .map(|s| (s.text, s.piped_stdin))
        .collect()
}

#[test]
fn default_rules_all_compile_and_match_count() {
    let rules = default_rules();
    assert_eq!(rules.len(), 11);
    for r in &rules {
        assert!(
            compile_rule(r).is_some(),
            "rule failed to compile: {}",
            r.pattern
        );
    }
}

#[test]
fn disabled_config_returns_none() {
    for mode in [InterceptMode::Hint, InterceptMode::Block] {
        let off = BashInterceptorConfig {
            enabled: false,
            mode,
            patterns: None,
        };
        assert_eq!(check(&off, TOOLS, "cat x"), None);
        assert_eq!(
            ci_poll_hint(&off, TOOLS, "gh run view 1", "run: in_progress"),
            None
        );
    }
}

#[test]
fn default_is_enabled_hint_mode() {
    let config = BashInterceptorConfig::default();
    assert!(config.enabled);
    assert_eq!(config.mode, InterceptMode::Hint);
    let parsed: BashInterceptorConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(parsed, config);
    let block: BashInterceptorConfig = serde_json::from_str(r#"{"mode":"block"}"#).unwrap();
    assert_eq!(block.mode, InterceptMode::Block);
    assert!(serde_json::from_str::<BashInterceptorConfig>(r#"{"mode":"warn"}"#).is_err());
}

#[test]
fn read_rule() {
    for c in [
        "cat notes.txt",
        "  head -n 5 a.rs",
        "tail -n 5 log",
        "less README",
        "more x",
    ] {
        assert_eq!(blocked_tool(c).as_deref(), Some("read"), "{c}");
    }
    for c in ["catalog x", "cat", "echo cat x", "bat x"] {
        assert_eq!(blocked_tool(c), None, "{c}");
    }
    // Writing through `cat` is not a read.
    assert_eq!(blocked_tool("cat > notes.md"), None);
    assert_eq!(blocked_tool("cat >> notes.md"), None);
}

#[test]
fn sed_line_range_print_is_read() {
    for c in [
        "sed -n 10,20p src/main.rs",
        "sed -n '1,40p' README.md",
        "sed -n \"5p\" a.txt",
    ] {
        assert_eq!(blocked_tool(c).as_deref(), Some("read"), "{c}");
    }
    for c in [
        "sed -n '/fn main/,/^}/p' src/main.rs",
        "sed -n 1,5p",
        "sed -n '1,40p' a.rs b.rs",
        "sed 's/a/b/' f",
    ] {
        assert_eq!(blocked_tool(c), None, "{c}");
    }
}

#[test]
fn grep_rule() {
    for c in [
        "grep -r foo .",
        "rg foo",
        "ripgrep foo",
        "ag foo",
        "ack foo",
    ] {
        assert_eq!(blocked_tool(c).as_deref(), Some("grep"), "{c}");
    }
    for c in ["rgb foo", "egrep x"] {
        assert_eq!(blocked_tool(c), None, "{c}");
    }
}

#[test]
fn glob_rule() {
    for c in [
        "find . -name '*.rs'",
        "fd --type f foo",
        "locate -iname x",
        "find . -maxdepth 1 -type f",
        "find . -glob '*.md'",
    ] {
        assert_eq!(blocked_tool(c).as_deref(), Some("glob"), "{c}");
    }
    for c in [
        "find . -print",
        "fdisk -l",
        "findstr x",
        "find . -name '*.pyc' -delete",
        "find src -name '*.rs' -exec wc -l {} +",
        "find . -type f -mtime -1",
        "find . -type f -newer Cargo.lock",
        "find . -name '*.log' -size +1M",
        "find . -type f -print0",
    ] {
        assert_eq!(blocked_tool(c), None, "{c}");
    }
}

#[test]
fn sed_perl_awk_rules() {
    for c in ["sed -i 's/a/b/' f", "sed --in-place=.bak s/a/b/ f"] {
        assert_eq!(blocked_tool(c).as_deref(), Some("edit"), "{c}");
    }
    for c in ["sed 's/a/b/' f", "sed -n '/x/p' f"] {
        assert_eq!(blocked_tool(c), None, "{c}");
    }
    for c in [
        "perl -pi -e 's/a/b/' f",
        "perl -i -pe 's/a/b/' f",
        "perl -ni -e x f",
    ] {
        assert_eq!(blocked_tool(c).as_deref(), Some("edit"), "{c}");
    }
    for c in ["perl -i.bak -pe 's/a/b/' f", "perl -w -i -pe x f"] {
        assert_eq!(blocked_tool(c).as_deref(), Some("edit"), "{c}");
    }
    for c in [
        "perl -e 'print 1'",
        "perl script.pl",
        "perl -ne 'print if /x-id/' f",
        "perl -pe 's/-i//' f",
        "perl -I lib t/run.pl",
    ] {
        assert_eq!(blocked_tool(c), None, "{c}");
    }
    assert_eq!(
        blocked_tool("awk -i inplace '{print}' f").as_deref(),
        Some("edit")
    );
    for c in ["awk '{print}' f", "awk -F: '{print $1}' f"] {
        assert_eq!(blocked_tool(c), None, "{c}");
    }
}

#[test]
fn redirect_rule_positive() {
    for c in [
        "echo hi > out.txt",
        "echo hi >> notes",
        "printf 'x' > file.txt",
        "echo hi >| out",
        "echo hi > $HOME/x",
        "echo hi > ~/x",
        "echo hi > /dev/null; echo hi > out.txt",
    ] {
        assert_eq!(blocked_tool(c).as_deref(), Some("write"), "{c}");
    }
    // A heredoc into a file is a write, not a read.
    assert_eq!(
        blocked_tool("cat << 'EOF' > notes.md").as_deref(),
        Some("write")
    );
}

#[test]
fn redirect_rule_negative_and_dev_null_exemption() {
    for c in [
        "echo \"a -> b\"",
        "echo hi > /dev/null",
        "echo hi 2>/dev/null",
        "echo hi > \"/dev/null\"",
        "echo hi > /dev/stderr",
        "echo hi >/dev/tty",
        "echo x >&2",
        "echo hi > /dev/null && echo bye",
        "echo a && b",
    ] {
        assert_eq!(blocked_tool(c), None, "{c}");
    }
}

#[test]
fn redirect_target_uses_js_ascii_word_class() {
    // JS `\w` is ASCII-only without the `u` flag, so a non-ASCII target is not a redirect.
    assert_eq!(blocked_tool("echo hi > é.txt"), None);
    assert_eq!(blocked_tool("echo hi > ébc"), None);
}

#[test]
fn nohup_and_background_rule() {
    for c in ["nohup node server.js", "node server.js &", "sleep 100 &"] {
        assert_eq!(blocked_tool(c).as_deref(), Some("bg"), "{c}");
    }
    for c in ["make && test", "echo a && b"] {
        assert_eq!(blocked_tool(c), None, "{c}");
    }
}

#[test]
fn service_rule() {
    for c in [
        "npm run dev",
        "bun dev",
        "pnpm start",
        "yarn dev --port 3000",
        "vite",
        "vite --port 3000",
        "next dev",
        "nodemon server.js",
        "lldb ./a.out",
        "gdb ./a.out",
        "docker compose up",
        "docker compose up --build",
    ] {
        assert_eq!(blocked_tool(c).as_deref(), Some("bg"), "{c}");
    }
    for c in [
        "docker compose up -d",
        "docker compose up --detach",
        "npm install",
        "npm run build",
    ] {
        assert_eq!(blocked_tool(c), None, "{c}");
    }
}

#[test]
fn tail_follow_is_a_service_not_a_read() {
    for c in ["tail -f app.log", "tail -F app.log"] {
        assert_eq!(blocked_tool(c).as_deref(), Some("bg"), "{c}");
    }
    assert_eq!(blocked_tool("tail -n 5 app.log").as_deref(), Some("read"));
    assert_eq!(blocked_tool_with(&on(), &["read"], "tail -f app.log"), None);
}

#[test]
fn watch_rule() {
    for c in [
        "cargo watch -x check -w src",
        "vitest --watch",
        "jest --watch",
        "tsc -w",
        "npm run build --watch",
        "npm run build\n--watch",
        "pytest --watch",
    ] {
        assert_eq!(blocked_tool(c).as_deref(), Some("bg"), "{c}");
    }
    for c in [
        "cargo watch -x test",
        "tsc --noEmit",
        "echo --watch",
        "pytest -q",
    ] {
        assert_eq!(blocked_tool(c), None, "{c}");
    }
}

#[test]
fn js_dot_excludes_carriage_return() {
    // JS `.` does not match `\r`, so `(?:.|\n)*` cannot span it.
    assert_eq!(blocked_tool("npm run build\r--watch"), None);
}

#[test]
fn pipe_stdin_segments_are_exempt() {
    assert_eq!(blocked_tool("printf x | cat"), None);
    assert_eq!(blocked_tool("echo data | grep foo"), None);
    assert_eq!(blocked_tool("ls | grep foo"), None);
    // The first stage of a pipeline is still a standalone command.
    assert_eq!(blocked_tool("cat file | wc -l").as_deref(), Some("read"));
}

#[test]
fn leading_assignments_are_stripped_for_matching() {
    assert_eq!(
        blocked_tool("FOO=1 BAR=\"x y\" grep foo file").as_deref(),
        Some("grep")
    );
    assert_eq!(blocked_tool("A=1 cat x | wc -l").as_deref(), Some("read"));
    assert_eq!(blocked_tool("FOO=1 ls"), None);
    assert_eq!(blocked_tool("FOO=1"), None);
}

#[test]
fn tool_availability_gates_rules() {
    assert_eq!(check(&on(), &["grep"], "cat x"), None);
    assert!(check(&on(), &["read"], "cat x").is_some());
    assert_eq!(check(&on(), &["bash"], "npm run dev"), None);
    assert_eq!(check(&hint_mode(), &["bash"], "grep -rn foo src"), None);
}

#[test]
fn custom_patterns_override_defaults() {
    let config = BashInterceptorConfig {
        enabled: true,
        patterns: Some(vec![rule(r"^\s*rm\s+", "bash", "no rm")]),
        ..on()
    };
    assert_eq!(
        blocked(&config, "rm -rf x").as_deref(),
        Some("Blocked: no rm\n\nOriginal command: rm -rf x")
    );
    assert_eq!(check(&config, TOOLS, "cat x"), None);
    let hinting = BashInterceptorConfig {
        mode: InterceptMode::Hint,
        ..config
    };
    assert_eq!(
        check(&hinting, TOOLS, "rm -rf x"),
        Some(Verdict::Hint("[hint] no rm".into()))
    );

    let empty = BashInterceptorConfig {
        patterns: Some(Vec::new()),
        ..on()
    };
    assert_eq!(check(&empty, TOOLS, "cat x"), None);
}

#[test]
fn invalid_regex_and_flags_are_skipped() {
    let config = BashInterceptorConfig {
        patterns: Some(vec![
            rule("(", "bash", "bad regex"),
            Rule {
                flags: Some("q".into()),
                ..rule("^x", "bash", "bad flag")
            },
            Rule {
                flags: Some("gg".into()),
                ..rule("^x", "bash", "dup flag")
            },
            Rule {
                flags: Some("i".into()),
                ..rule("^cat", "read", "ci")
            },
        ]),
        ..on()
    };
    assert_eq!(
        blocked(&config, "CAT x").as_deref(),
        Some("Blocked: ci\n\nOriginal command: CAT x")
    );
}

#[test]
fn sticky_and_multiline_flags() {
    let sticky = BashInterceptorConfig {
        patterns: Some(vec![Rule {
            flags: Some("y".into()),
            ..rule("cat", "read", "sticky")
        }]),
        ..on()
    };
    assert!(check(&sticky, TOOLS, "cat x").is_some());
    assert!(check(&sticky, TOOLS, "x cat").is_none());

    let multi = BashInterceptorConfig {
        patterns: Some(vec![Rule {
            flags: Some("m".into()),
            ..rule("^cat", "read", "multi")
        }]),
        ..on()
    };
    // Candidates are split on `\n`, so this also works via the whole command.
    assert!(check(&multi, TOOLS, "ls\ncat x").is_some());
}

#[test]
fn config_and_rule_serde_are_camel_case_and_strict() {
    let rule: Rule = serde_json::from_str(
        r#"{"pattern":"^x","tool":"read","message":"m","allowSubcommands":["a"]}"#,
    )
    .unwrap();
    assert_eq!(rule.allow_subcommands, Some(vec!["a".to_owned()]));
    assert!(
        serde_json::from_str::<Rule>(r#"{"pattern":"^x","tool":"read","message":"m","bogus":1}"#)
            .is_err()
    );

    let config: BashInterceptorConfig = serde_json::from_str(r#"{"enabled":true}"#).unwrap();
    assert!(config.enabled);
    assert_eq!(config.patterns, None);
    assert!(
        serde_json::from_str::<BashInterceptorConfig>(r#"{"enabled":true,"extra":1}"#).is_err()
    );
}

#[test]
fn hint_mode_hints_a_lone_duplicate() {
    for (c, tool) in [
        ("grep -rn \"fn check\" src", "grep"),
        ("rg -n foo src", "grep"),
        ("cat Cargo.toml", "read"),
        ("sed -n 120,185p src/tools/execution.rs", "read"),
        ("find . -name '*.rs' -type f", "glob"),
        ("sed -i '' 's/a/b/' src/lib.rs", "edit"),
        ("npm run dev", "bg"),
        ("node server.js &", "bg"),
        // A leading `cd DIR` and output trimming do not change what the command is.
        ("cd crates/core && grep -rn foo src", "grep"),
        ("grep -n \"QListWidget\" src/ui/settings.py | head", "grep"),
        ("cat log.txt | tail -n 40", "read"),
        ("RUST_LOG=debug cat x", "read"),
    ] {
        assert_eq!(hinted_tool(c).as_deref(), Some(tool), "{c}");
    }
}

#[test]
fn hint_mode_leaves_batched_and_scripted_commands_alone() {
    // Commands taken from session transcripts: batching several reads into one call saves
    // model round trips, and piping into another program is shell work a tool cannot do.
    for c in [
        "sed -n 30,60p src/ui/overlay/settings.rs; sed -n 285,345p src/ui/overlay/settings.rs",
        "grep -n \"set_title_fields\" src/ui/main_page.py; grep -n \"def title_fields\" src/ui/settings.py",
        "grep '^version' pyproject.toml && grep '__version__' src/simple_stipple/__init__.py",
        "grep -ohE \"page\\.[a-z_]+\" tests/test_shipping_ui.py | sort | uniq -c | sort -rn",
        "cat file | wc -l",
        "find . -name '*.rs' | xargs wc -l",
        "sed -i '' 's/a/b/' .tmp/audit.py && .venv/bin/python .tmp/audit.py",
        "cat > .tmp/set.py <<'EOF'\nprint(1)\nEOF",
        "tail -f app.log | grep ERROR",
        "grep -q foo f && echo yes",
        "cd src && ls",
        "ls -la",
        "cargo test --lib bash_interceptor",
        "git grep -n foo",
    ] {
        assert_eq!(check(&hint_mode(), TOOLS, c), None, "{c}");
    }
    // Block mode still inspects every standalone segment.
    assert!(matches!(
        check(&on(), TOOLS, "cargo build; grep -rn foo src"),
        Some(Verdict::Block(_))
    ));
}

#[test]
fn hint_is_one_line() {
    for c in [
        "grep -rn foo src",
        "cat x",
        "npm run dev",
        "sed -i s/a/b/ f",
    ] {
        let Some(Verdict::Hint(line)) = check(&hint_mode(), TOOLS, c) else {
            panic!("expected a hint for {c}");
        };
        assert!(
            line.starts_with("[hint] ") && !line.contains('\n'),
            "{line}"
        );
    }
}

#[test]
fn trim_hint_flags_a_head_tail_over_a_plain_command_even_in_a_batch() {
    let config = hint_mode();
    for c in [
        "cargo clippy --all-targets 2>&1 | tail -30",
        "cat package.json | head -50; ls",
        "ls; make lint 2>&1 | tail -15; cargo fmt --check 2>&1 | head -20",
    ] {
        let line = trim_hint(&config, c).unwrap_or_else(|| panic!("expected a hint: {c}"));
        assert!(
            line.starts_with("[hint] ") && !line.contains('\n'),
            "{line}"
        );
    }
}

#[test]
fn trim_hint_leaves_selection_followed_streams_and_untrimmed_commands_alone() {
    let config = hint_mode();
    for c in [
        "grep -oh 'page[a-z_]*' f | sort | uniq -c | sort -rn | head -5",
        "ls | wc -l",
        "head -5 file; ls",
        "cargo build 2>&1",
        "tail -f app.log",
        "tail -f app.log | grep ERROR",
        "echo $(date) | head",
    ] {
        assert_eq!(trim_hint(&config, c), None, "{c}");
    }
    let off = BashInterceptorConfig {
        enabled: false,
        ..hint_mode()
    };
    assert_eq!(trim_hint(&off, "make 2>&1 | tail -5"), None);
}

#[test]
fn ci_poll_hint_fires_on_unfinished_status_or_sleep_loop() {
    let config = hint_mode();
    let view = "gh run view 38768263506 -R KiidxAtlas/dial --json status,conclusion,jobs --jq '\"run: \\(.status) \\(.conclusion)\"'";
    let running = "run: in_progress \nBuild and package Windows release: in_progress \n";
    let hint = ci_poll_hint(&config, TOOLS, view, running).expect("in_progress view");
    assert!(
        hint.contains("gh run watch") && hint.contains("bg"),
        "{hint}"
    );
    assert!(!hint.contains('\n'));
    assert!(
        ci_poll_hint(&config, &["github"], view, running)
            .is_some_and(|hint| hint.contains("run_watch"))
    );
    assert!(
        ci_poll_hint(&config, &[], view, running)
            .is_some_and(|hint| hint.contains("gh run watch") && !hint.contains("`bg`"))
    );
    for (command, output) in [
        (
            "gh run list -L 3",
            "in_progress\t\tfix ci\tci\tmain\tpush\t1\t39s\n",
        ),
        ("gh pr checks 12", "build\tpending\t0\thttps://x\n"),
        (
            "gh run list -L 3 --json status",
            "[{\"status\":\"queued\"}]",
        ),
        (
            "for i in $(seq 1 19); do s=$(gh run view 1 --json status --jq .status); [ \"$s\" = completed ] && break; sleep 15; done",
            "command is running as background job bg20 (after 15s)",
        ),
    ] {
        assert!(
            ci_poll_hint(&config, TOOLS, command, output).is_some(),
            "{command}"
        );
    }
}

#[test]
fn ci_poll_hint_ignores_finished_runs_logs_and_watches() {
    let config = hint_mode();
    for (command, output) in [
        ("gh run view 1 --json status", "run: completed success\n"),
        ("gh run view 1 --log-failed", "step in_progress marker\n"),
        (
            "gh run watch 1 --exit-status > /dev/null; gh run view 1",
            "command is running as background job bg3",
        ),
        ("gh pr view 12", "state: OPEN pending review\n"),
        (
            "sleep 10; id=$(gh run list --limit 1 --json databaseId -q '.[0].databaseId'); gh run watch $id --exit-status",
            "command is running as background job bg5",
        ),
        ("cargo test", "test in_progress ... ok\n"),
    ] {
        assert_eq!(
            ci_poll_hint(&config, TOOLS, command, output),
            None,
            "{command}"
        );
    }
}

#[test]
fn flat_segments_split_pipes_and_lists() {
    assert_eq!(
        segments("a; b | c && d"),
        vec![
            ("a".to_owned(), false),
            ("b".to_owned(), false),
            ("c".to_owned(), true),
            ("d".to_owned(), false),
        ]
    );
}

#[test]
fn flat_segments_respect_quotes() {
    assert_eq!(
        segments("echo \"a;b\" && c"),
        vec![("echo \"a;b\"".to_owned(), false), ("c".to_owned(), false)]
    );
}

#[test]
fn flat_segments_keep_fd_duplication_and_stderr_pipe() {
    assert_eq!(
        segments("a 2>&1 | cat"),
        vec![("a 2>&1".to_owned(), false), ("cat".to_owned(), true)]
    );
    assert_eq!(
        segments("a |& b"),
        vec![("a".to_owned(), false), ("b".to_owned(), true)]
    );
}

#[test]
fn flat_segments_comment_is_a_boundary() {
    assert_eq!(
        segments("a # note\nb"),
        vec![("a".to_owned(), false), ("b".to_owned(), false)]
    );
}

#[test]
fn flat_segments_decline_unscannable_syntax() {
    assert!(segments("cat <<EOF").is_empty());
    assert!(segments("echo $(ls)").is_empty());
    assert!(segments("echo `ls`").is_empty());
    assert!(segments("(cd x; ls)").is_empty());
    assert!(segments("echo 'x").is_empty());
    assert!(segments("echo \\").is_empty());
}

#[test]
fn leading_assignment_stripping() {
    assert_eq!(
        without_leading_environment_assignments("FOO=\"a b\" cmd arg").as_deref(),
        Some("cmd arg")
    );
    assert_eq!(without_leading_environment_assignments("FOO=\"a"), None);
    assert_eq!(without_leading_environment_assignments("FOO=1"), None);
    assert_eq!(without_leading_environment_assignments("ls -la"), None);
}
