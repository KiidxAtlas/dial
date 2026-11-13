//! Engine-level tests for `xargs` and `find -exec`. Each case runs a script in a real embedded shell
//! with the bundled builtins registered, so resolution, redirection, exit status and argument
//! passing are checked as a user sees them. Output goes to files named by the script, which keeps
//! the tests free of host stdout and identical across platforms.

use std::fs;
use std::path::Path;

use crate::embedded_shell::builtins::tests::run_script;
use tempfile::{TempDir, tempdir};

fn write(dir: &Path, name: &str, text: &str) {
    let path = dir.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn read(dir: &Path, name: &str) -> String {
    fs::read_to_string(dir.join(name)).unwrap()
}

/// Runs each `(script, expected out.txt)` case in its own directory and expects status 0.
fn check_output_cases(cases: &[(&str, &str)]) {
    for (script, expected) in cases {
        let dir = tempdir().unwrap();
        assert_eq!(run_script(dir.path(), script), 0, "script: {script}");
        assert_eq!(read(dir.path(), "out.txt"), *expected, "script: {script}");
    }
}

#[test]
fn xargs_targets_functions_and_bundled_builtins_and_ignores_path() {
    check_output_cases(&[
        (
            r#"export PATH=/nonexistent
f() { echo "fn:$1"; }
printf 'a\nb\n' | xargs -n1 f > out.txt"#,
            "fn:a\nfn:b\n",
        ),
        (
            r"printf 'a b\0c\0' | xargs -0 -n1 echo > out.txt",
            "a b\nc\n",
        ),
        (r"printf 'x\n' | xargs -I{} echo '<{}>' > out.txt", "<x>\n"),
        (r"printf 'ab cd\n' | xargs -n1 echo > out.txt", "ab\ncd\n"),
    ]);
}

#[test]
fn xargs_passes_words_as_values_never_as_shell_text() {
    check_output_cases(&[(
        r"printf '$HOME;rm -rf x\n' | xargs -n1 echo > out.txt",
        "$HOME;rm\n-rf\nx\n",
    )]);
}

#[test]
fn xargs_quoting_groups_words_like_gnu() {
    check_output_cases(&[(
        r#"printf '"a b" c\n' | xargs -n1 echo > out.txt"#,
        "a b\nc\n",
    )]);
}

#[test]
fn xargs_empty_input_runs_once_unless_no_run_if_empty() {
    check_output_cases(&[
        (r"printf '' | xargs echo ran > out.txt", "ran\n"),
        (r"printf '' | xargs -r echo ran > out.txt", ""),
    ]);
}

#[test]
fn xargs_exit_status_follows_gnu() {
    let cases: &[(&str, u8)] = &[
        ("f() { return 1; }\nprintf 'a\\n' | xargs -n1 f", 123),
        ("g() { return 255; }\nprintf 'a\\n' | xargs -n1 g", 124),
        ("printf 'a\\n' | xargs -n1 false", 123),
        ("printf 'a\\n' | xargs true", 0),
        ("printf 'a\\n' | xargs nosuch-command-xyz", 127),
    ];
    for (script, status) in cases {
        let dir = tempdir().unwrap();
        assert_eq!(run_script(dir.path(), script), *status, "script: {script}");
    }
}

#[test]
fn xargs_redirects_command_streams_and_reports_missing_commands() {
    let dir = tempdir().unwrap();
    let script = r#"h() { echo "out-$1"; echo "err-$1" >&2; }
printf 'a\n' | xargs -n1 h > out.txt 2> err.txt
printf 'a\n' | xargs nosuch-command-xyz 2> missing.txt"#;
    assert_eq!(run_script(dir.path(), script), 127);
    assert_eq!(read(dir.path(), "out.txt"), "out-a\n");
    assert_eq!(read(dir.path(), "err.txt"), "err-a\n");
    assert_eq!(
        read(dir.path(), "missing.txt"),
        "xargs: nosuch-command-xyz: command not found\n"
    );
}

#[test]
fn xargs_commands_run_in_a_copy_of_the_shell() {
    let dir = tempdir().unwrap();
    let script = r#"f() { cd /; X=set; }
pwd > before.txt
printf 'a\n' | xargs -n1 f
pwd > after.txt
echo "[$X]" > out.txt"#;
    assert_eq!(run_script(dir.path(), script), 0);
    assert_eq!(
        read(dir.path(), "before.txt"),
        read(dir.path(), "after.txt")
    );
    assert_eq!(read(dir.path(), "out.txt"), "[]\n");
}

#[test]
fn xargs_runs_commands_in_the_shell_working_directory() {
    let dir = tempdir().unwrap();
    write(dir.path(), "f.txt", "content");
    assert_eq!(
        run_script(dir.path(), "printf 'f.txt\\n' | xargs cat > out.txt"),
        0
    );
    assert_eq!(read(dir.path(), "out.txt"), "content");
}

#[cfg(unix)]
#[test]
fn bundled_commands_take_precedence_over_path() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempdir().unwrap();
    write(dir.path(), "a.txt", "real");
    write(dir.path(), "bin/cat", "#!/bin/sh\necho fake\n");
    fs::set_permissions(
        dir.path().join("bin/cat"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let script = r#"export PATH="$PWD/bin:$PATH"
printf 'a.txt\n' | xargs cat > out.txt"#;
    assert_eq!(run_script(dir.path(), script), 0);
    assert_eq!(read(dir.path(), "out.txt"), "real");
}

/// Layout shared by the `find` cases: nested files, one with a space in its name.
fn find_tree() -> TempDir {
    let dir = tempdir().unwrap();
    write(dir.path(), "root/a.txt", "A");
    write(dir.path(), "root/c d.txt", "C");
    write(dir.path(), "root/sub/b.txt", "B");
    dir
}

#[test]
fn find_exec_runs_commands_per_match_and_suppresses_default_print() {
    let dir = find_tree();
    let cases: &[(&str, &str)] = &[
        (r"find root -name '*.txt' -exec cat {} \; > out.txt", "ACB"),
        (r"find root -name a.txt -exec true {} \; > out.txt", ""),
        (
            r#"h() { echo "h:$1"; }
find root -name a.txt -exec h {} \; > out.txt"#,
            "h:root/a.txt\n",
        ),
    ];
    for (script, expected) in cases {
        assert_eq!(run_script(dir.path(), script), 0, "script: {script}");
        assert_eq!(read(dir.path(), "out.txt"), *expected, "script: {script}");
    }
}

#[test]
fn find_exec_plus_batches_all_paths_into_one_command() {
    let dir = find_tree();
    assert_eq!(
        run_script(
            dir.path(),
            r"find root -name '*.txt' -exec echo {} + > out.txt"
        ),
        0
    );
    assert_eq!(
        read(dir.path(), "out.txt"),
        "root/a.txt root/c d.txt root/sub/b.txt\n"
    );
}

#[test]
fn find_exec_failures_set_status_one() {
    let dir = find_tree();
    assert_eq!(
        run_script(
            dir.path(),
            r"find root -name a.txt -exec nosuch-command-xyz {} \;"
        ),
        1
    );
}

#[test]
fn find_print0_feeds_xargs_names_with_spaces() {
    let dir = find_tree();
    assert_eq!(
        run_script(
            dir.path(),
            r"find root -type f -print0 | xargs -0 -n1 echo > out.txt"
        ),
        0
    );
    assert_eq!(
        read(dir.path(), "out.txt"),
        "root/a.txt\nroot/c d.txt\nroot/sub/b.txt\n"
    );
}

#[cfg(unix)]
#[test]
fn find_print0_and_xargs_keep_newlines_in_names() {
    let dir = find_tree();
    write(dir.path(), "root/n\nl.txt", "");
    assert_eq!(
        run_script(
            dir.path(),
            r"find root -type f -print0 | xargs -0 -n1 cat > out.txt"
        ),
        0
    );
    assert_eq!(read(dir.path(), "out.txt"), "ACB");
    assert_eq!(
        run_script(
            dir.path(),
            r"find root -type f -print0 | xargs -0 -n1 echo > out.txt"
        ),
        0
    );
    assert_eq!(
        read(dir.path(), "out.txt"),
        "root/a.txt\nroot/c d.txt\nroot/n\nl.txt\nroot/sub/b.txt\n"
    );
}

#[test]
fn head_bytes_leaves_the_rest_of_a_shared_stdin_to_the_next_command() {
    check_output_cases(&[(
        r"printf abcdefgh | { head -c2; cat; } > out.txt",
        "abcdefgh",
    )]);
}

#[test]
fn find_exec_plus_status_reflects_the_batch_command_but_semicolon_does_not() {
    let dir = find_tree();
    let cases: &[(&str, u8)] = &[
        (r"find root -name '*.txt' -exec false {} \;", 0),
        (r"find root -name '*.txt' -exec false {} +", 1),
        (r"find root -name '*.txt' -exec true {} +", 0),
    ];
    for (script, status) in cases {
        assert_eq!(run_script(dir.path(), script), *status, "script: {script}");
    }
}

/// The bundled `find` declines these; with no `find` on `PATH` the usage status stands.
#[test]
fn find_rejects_repeated_and_late_tests_with_usage_status() {
    let dir = find_tree();
    for script in [
        r"PATH=/nonexistent; find root -name a -name b > out.txt",
        r"PATH=/nonexistent; find root -print -name a > out.txt",
    ] {
        assert_eq!(run_script(dir.path(), script), 2, "script: {script}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn find_reports_non_utf8_names_instead_of_running_commands_on_another_path() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let dir = find_tree();
    fs::write(
        dir.path()
            .join("root")
            .join(OsStr::from_bytes(b"bad\xff.txt")),
        "",
    )
    .unwrap();
    let script = r"find root -name '*.txt' -exec echo {} \; > out.txt";
    assert_eq!(run_script(dir.path(), script), 1);
    let out = read(dir.path(), "out.txt");
    assert!(out.contains("root/a.txt"), "out: {out:?}");
    assert!(!out.contains("bad"), "out: {out:?}");
}
