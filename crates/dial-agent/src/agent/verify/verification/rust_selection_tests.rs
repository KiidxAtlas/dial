use super::*;
use std::fs;

// Fixtures are written to a temp workspace; selection only reads manifests and sources, never runs cargo.

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn plan_for(root: &Path, changed: &[&str]) -> RelatedTestPlan {
    let changed: Vec<PathBuf> = changed.iter().map(|path| PathBuf::from(*path)).collect();
    related_test_plan_with_runner(root, Path::new("python3"), &changed, &[])
}

fn commands(plan: &RelatedTestPlan) -> Vec<&str> {
    plan.checks
        .iter()
        .map(|check| check.command.as_str())
        .collect()
}

fn gap_mentions(plan: &RelatedTestPlan, needle: &str) -> bool {
    plan.limits.iter().any(|limit| limit.contains(needle))
}

#[test]
fn workspace_member_inline_tests_use_their_own_manifest_and_lib_target() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/*\"]\n",
    );
    write(
        root,
        "crates/core/Cargo.toml",
        "[package]\nname = \"core-lib\"\n",
    );
    write(root, "crates/core/src/lib.rs", "pub mod parser;\n");
    write(
        root,
        "crates/core/src/parser.rs",
        "pub fn parse() {}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn parses() {}\n}\n",
    );
    let plan = plan_for(root, &["crates/core/src/parser.rs"]);
    assert_eq!(
        commands(&plan),
        vec![
            "cargo test --offline --manifest-path 'crates/core/Cargo.toml' --lib 'parser::tests::' --quiet -j1"
        ]
    );
    assert!(plan.limits.is_empty(), "{:?}", plan.limits);
}

#[test]
fn external_tests_module_and_its_file_select_the_same_filter_once() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(root, "Cargo.toml", "[package]\nname = \"pkg\"\n");
    write(root, "src/lib.rs", "pub mod agent;\n");
    write(
        root,
        "src/agent/parser.rs",
        "pub fn p() {}\n#[cfg(test)]\nmod tests;\n",
    );
    write(
        root,
        "src/agent/parser/tests.rs",
        "#[test]\nfn external() {}\n",
    );
    let plan = plan_for(root, &["src/agent/parser.rs", "src/agent/parser/tests.rs"]);
    assert_eq!(plan.checks.len(), 1, "{:?}", plan.checks);
    assert!(
        plan.checks[0]
            .command
            .ends_with("--lib 'agent::parser::tests::' --quiet -j1"),
        "{}",
        plan.checks[0].command
    );
    assert!(plan.limits.is_empty(), "{:?}", plan.limits);
}

#[test]
fn integration_targets_filter_by_module_and_fall_back_to_the_whole_target() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(root, "Cargo.toml", "[package]\nname = \"svc\"\n");
    write(
        root,
        "tests/integration/main.rs",
        "mod agent;\nmod support;\n",
    );
    write(
        root,
        "tests/integration/agent.rs",
        "#[test]\nfn runs() {}\n",
    );
    write(root, "tests/integration/support.rs", "pub fn helper() {}\n");
    write(root, "tests/api.rs", "#[test]\nfn api() {}\n");

    assert_eq!(
        commands(&plan_for(root, &["tests/integration/agent.rs"])),
        vec![
            "cargo test --offline --manifest-path 'Cargo.toml' --test 'integration' 'agent::' --quiet -j1"
        ]
    );
    // A crate root and a helper without its own tests run the whole integration target.
    assert_eq!(
        commands(&plan_for(root, &["tests/integration/main.rs"])),
        vec!["cargo test --offline --manifest-path 'Cargo.toml' --test 'integration' --quiet -j1"]
    );
    assert_eq!(
        commands(&plan_for(root, &["tests/integration/support.rs"])),
        vec!["cargo test --offline --manifest-path 'Cargo.toml' --test 'integration' --quiet -j1"]
    );
    assert_eq!(
        commands(&plan_for(root, &["tests/api.rs"])),
        vec!["cargo test --offline --manifest-path 'Cargo.toml' --test 'api' --quiet -j1"]
    );
}

#[test]
fn bin_only_package_selects_its_binary_and_discloses_test_free_roots() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(root, "Cargo.toml", "[package]\nname = \"tool-cli\"\n");
    write(root, "src/main.rs", "mod cli;\nfn main() {}\n");
    write(
        root,
        "src/cli/args.rs",
        "#[cfg(test)]\nmod tests {\n    #[test]\n    fn a() {}\n}\n",
    );
    write(root, "src/bin/extra.rs", "fn main() {}\n");

    let plan = plan_for(root, &["src/cli/args.rs"]);
    assert_eq!(
        commands(&plan),
        vec![
            "cargo test --offline --manifest-path 'Cargo.toml' --bin 'tool-cli' 'cli::args::tests::' --quiet -j1"
        ]
    );
    let plan = plan_for(root, &["src/bin/extra.rs"]);
    assert!(plan.checks.is_empty(), "{:?}", plan.checks);
    assert!(gap_mentions(&plan, "src/bin/extra.rs"), "{:?}", plan.limits);
}

#[test]
fn lib_and_bin_share_src_and_the_lib_wins_the_tie() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(root, "Cargo.toml", "[package]\nname = \"both\"\n");
    write(root, "src/lib.rs", "pub mod shared;\n");
    write(root, "src/main.rs", "fn main() {}\n");
    write(root, "src/shared.rs", "#[test]\nfn s() {}\n");
    assert_eq!(
        commands(&plan_for(root, &["src/shared.rs"])),
        vec!["cargo test --offline --manifest-path 'Cargo.toml' --lib 'shared::' --quiet -j1"]
    );
}

#[test]
fn declared_targets_replace_auto_discovery_when_disabled() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        "Cargo.toml",
        "[package]\nname = \"decl\"\nautotests = false\n\n[[test]]\nname = \"smoke\"\npath = \"qa/smoke.rs\"\n",
    );
    write(root, "qa/smoke.rs", "#[test]\nfn s() {}\n");
    write(root, "tests/loose.rs", "#[test]\nfn x() {}\n");

    assert_eq!(
        commands(&plan_for(root, &["qa/smoke.rs"])),
        vec!["cargo test --offline --manifest-path 'Cargo.toml' --test 'smoke' --quiet -j1"]
    );
    let plan = plan_for(root, &["tests/loose.rs"]);
    assert!(plan.checks.is_empty(), "{:?}", plan.checks);
    assert!(
        gap_mentions(&plan, "registered target"),
        "{:?}",
        plan.limits
    );
}

#[test]
fn unowned_virtual_and_malformed_manifests_are_gaps_not_passes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let bare = tempfile::tempdir().unwrap();
    write(bare.path(), "loose.rs", "#[test]\nfn l() {}\n");
    write(root, "Cargo.toml", "[workspace]\nmembers = []\n");
    write(root, "src/x.rs", "#[test]\nfn x() {}\n");
    write(root, "crates/bad/Cargo.toml", "[package\n");
    write(root, "crates/bad/src/lib.rs", "");
    write(root, "crates/bad/src/a.rs", "#[test]\nfn a() {}\n");

    let plan = plan_for(bare.path(), &["loose.rs"]);
    assert!(plan.checks.is_empty());
    assert!(
        gap_mentions(&plan, "no owning Cargo package"),
        "{:?}",
        plan.limits
    );

    let plan = plan_for(root, &["src/x.rs"]);
    assert!(plan.checks.is_empty());
    assert!(gap_mentions(&plan, "not a package"), "{:?}", plan.limits);

    let plan = plan_for(root, &["crates/bad/src/a.rs"]);
    assert!(plan.checks.is_empty());
    assert!(gap_mentions(&plan, "unreadable"), "{:?}", plan.limits);
}

#[test]
fn deleted_and_helper_paths_disclose_their_gaps() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(root, "Cargo.toml", "[package]\nname = \"pkg\"\n");
    write(root, "src/lib.rs", "pub mod kept;\n");
    write(root, "src/kept.rs", "#[test]\nfn k() {}\n");
    write(root, "tests/common/mod.rs", "pub fn shared() {}\n");

    let plan = plan_for(root, &["src/gone.rs"]);
    assert!(plan.checks.is_empty());
    assert!(gap_mentions(&plan, "deleted"), "{:?}", plan.limits);

    // A shared helper in a subdirectory has no single owning target.
    let plan = plan_for(root, &["tests/common/mod.rs"]);
    assert!(plan.checks.is_empty());
    assert!(
        gap_mentions(&plan, "tests/common/mod.rs"),
        "{:?}",
        plan.limits
    );
}

#[test]
fn plain_modules_and_non_identifier_names_are_disclosed() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(root, "Cargo.toml", "[package]\nname = \"pkg\"\n");
    write(root, "src/lib.rs", "pub mod plain;\n");
    write(root, "src/plain.rs", "pub fn no_tests() {}\n");
    write(root, "src/my-file.rs", "#[test]\nfn m() {}\n");

    let plan = plan_for(root, &["src/plain.rs"]);
    assert!(plan.checks.is_empty());
    assert!(
        gap_mentions(&plan, "no tests in its module"),
        "{:?}",
        plan.limits
    );

    let plan = plan_for(root, &["src/my-file.rs"]);
    assert!(plan.checks.is_empty());
    assert!(
        gap_mentions(&plan, "plain Rust identifier"),
        "{:?}",
        plan.limits
    );
}

#[test]
fn package_directories_with_spaces_are_quoted_in_manifest_and_filter() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        "my crates/app/Cargo.toml",
        "[package]\nname = \"app\"\n",
    );
    write(root, "my crates/app/src/lib.rs", "pub mod a;\n");
    write(root, "my crates/app/src/a.rs", "#[test]\nfn a() {}\n");
    let plan = plan_for(root, &["my crates/app/src/a.rs"]);
    assert_eq!(
        commands(&plan),
        vec![
            "cargo test --offline --manifest-path 'my crates/app/Cargo.toml' --lib 'a::' --quiet -j1"
        ]
    );
    assert_eq!(
        plan.checks[0].name,
        "related Rust tests: my crates/app --lib a::"
    );
}

#[test]
fn absolute_and_parent_paths_never_reach_the_filesystem() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(root, "Cargo.toml", "[package]\nname = \"pkg\"\n");
    write(root, "src/lib.rs", "");
    let plan = plan_for(root, &["../outside.rs", "/etc/passwd.rs"]);
    assert!(plan.checks.is_empty());
    assert_eq!(plan.limits.len(), 2, "{:?}", plan.limits);
}

#[test]
fn path_only_coverage_limits_cover_build_scripts_benches_and_manifests() {
    let limits = coverage_limits(&[
        PathBuf::from("build.rs"),
        PathBuf::from("crates/x/benches/b.rs"),
        PathBuf::from("Cargo.toml"),
        PathBuf::from("src/a.rs"),
    ]);
    assert_eq!(limits.len(), 3, "{limits:?}");
    assert!(limits[0].contains("build script"));
    assert!(limits[1].contains("benchmark"));
    assert!(limits[2].contains("manifest"));
}

#[test]
fn rust_selection_does_not_duplicate_path_only_limits() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(root, "Cargo.toml", "[package]\nname = \"pkg\"\n");
    write(root, "src/lib.rs", "");
    write(root, "build.rs", "fn main() {}\n");
    let plan = plan_for(root, &["build.rs"]);
    assert!(plan.checks.is_empty());
    assert_eq!(plan.limits.len(), 1, "{:?}", plan.limits);
}
