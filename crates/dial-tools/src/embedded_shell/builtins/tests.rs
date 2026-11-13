use super::*;
use std::path::Path;

use brush_builtins::{BuiltinSet, ShellBuilderExt};
use brush_core::{ProfileLoadBehavior, RcLoadBehavior, SourceInfo};

/// Runs `script` in a fresh embedded shell whose working directory is `dir`. Returns the exit
/// status of the script's last command.
pub(super) fn run_script(dir: &Path, script: &str) -> u8 {
    // Parallel shells each hold a runtime, pipes and redirect files; macOS's default 256
    // descriptors run out (`Too many open files`), which surfaces as exit 126 or lost output.
    crate::llm::fake::raise_fd_limit();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut shell = brush_core::Shell::builder()
            .default_builtins(BuiltinSet::BashMode)
            .builtins(crate::embedded_shell::builtins::registrations())
            .profile(ProfileLoadBehavior::Skip)
            .rc(RcLoadBehavior::Skip)
            .interactive(false)
            .shell_name("bash".to_owned())
            .build()
            .await
            .unwrap();
        let params = shell.default_exec_params();
        let cwd = dir.display().to_string().replace('\'', r"'\''");
        let script = format!("cd '{cwd}' || exit 3\n{script}");
        let result = shell
            .run_string(script, &SourceInfo::default(), &params)
            .await
            .unwrap();
        u8::from(&result.exit_code)
    })
}

#[test]
fn every_command_has_a_unique_name_and_synopsis() {
    let mut names: Vec<&str> = UTILS.iter().map(|u| u.name).collect();
    names.sort_unstable();
    let unique = names.len();
    names.dedup();
    assert_eq!(names.len(), unique);
    assert!(UTILS.iter().all(|u| u.synopsis.starts_with(u.name)));
}

#[test]
fn registrations_cover_every_command() {
    assert_eq!(registrations().len(), UTILS.len());
}

/// Dial's `kill` replaces brush's on every OS: it sends `TERM` by default, as bash does.
#[cfg(unix)]
#[test]
fn kill_sends_term_by_default_and_reports_missing_processes() {
    let dir = tempfile::tempdir().unwrap();
    let script = r#"sleep 30 & p=$!
kill -0 "$p" || exit 10
kill "$p"; wait "$p"
[ $? -eq 143 ] || exit 11
kill "$p" 2>/dev/null && exit 12
[ "$(kill -l TERM)" = 15 ] || exit 13
kill -s KILL nope 2>/dev/null; [ $? -eq 2 ] || exit 14"#;
    assert_eq!(run_script(dir.path(), script), 0);
}

/// Options outside a bundled command's subset run the `PATH` program of the same name with the
/// unchanged arguments, standard input and redirections. Without one, the usage error stands.
#[cfg(unix)]
#[test]
fn declined_options_run_the_path_program_or_report_usage() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let fake = bin.join("grep");
    std::fs::write(
        &fake,
        "#!/bin/sh\nread line\necho \"system grep $* <$line>\"\nexit 7\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let script = r#"PATH="$PWD/bin"
out=$(echo input | grep -P 'a b')
[ $? -eq 7 ] || exit 10
[ "$out" = "system grep -P a b <input>" ] || exit 11
[ "$(echo abc | grep b)" = abc ] || exit 12
PATH=/nonexistent
grep -P x </dev/null 2>err; [ $? -eq 2 ] || exit 13
[ "$(head -1 err)" = "grep: invalid option -- 'P'" ] || exit 14
sed -i s/a/b/ nofile 2>/dev/null; [ $? -eq 2 ] || exit 15"#;
    assert_eq!(run_script(dir.path(), script), 0);
}

/// Common options that the agent's own checks use: `grep -x/-w/-s`, `cp -f`, `mv -f`, `rm -R`,
/// `tail -n +N`.
#[test]
fn common_coreutils_options_are_bundled() {
    let dir = tempfile::tempdir().unwrap();
    let script = r#"PATH=/nonexistent
printf 'ab\na\nab a\nfoo_a\n' > f
[ "$(grep -x a f)" = a ] || exit 10
[ "$(grep -c -w a f)" = 2 ] || exit 11
[ "$(grep -o -w a f | wc -l)" -eq 2 ] || exit 12
[ "$(echo 'x-a-y' | grep -o -w a)" = a ] || exit 13
grep -s a missing 2>err; [ $? -eq 2 ] || exit 14
[ ! -s err ] || exit 15
cp -f f g && mv -f g h && [ -f h ] && [ ! -e g ] || exit 16
mkdir -p d/e && rm -R d && [ ! -e d ] || exit 17
[ "$(tail -n +3 f)" = "$(printf 'ab a\nfoo_a')" ] || exit 18
[ "$(tail -n+4 f)" = foo_a ] || exit 19"#;
    assert_eq!(run_script(dir.path(), script), 0);
}
