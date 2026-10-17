use super::*;

use crate::tools::gh_common::test_support::GhOverride;

/// Scoped inputs for PR test commands, never process-global environment mutations.
struct FakeEnv {
    previous: Option<(PathBuf, PathBuf)>,
}

impl FakeEnv {
    fn install(bin: &Path, home: &Path) -> Self {
        let previous =
            TEST_ENV.with(|value| value.replace(Some((bin.to_owned(), home.to_owned()))));
        Self { previous }
    }
}

impl Drop for FakeEnv {
    fn drop(&mut self) {
        TEST_ENV.with(|value| value.replace(self.previous.take()));
    }
}

fn write_script(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    // Windows has no file mode bits, so the chmod step is skipped there; the fake is a
    // POSIX shell script and only runs on Unix.
}

/// Fake `gh`: records argv (one line per call) and answers `pr create` and `pr view`. The
/// returned guard routes `run_gh` on this thread to it.
fn fake_gh(bin: &Path, log: &Path) -> GhOverride {
    write_script(
        bin,
        "gh",
        &format!(
            r#"printf '%s\n' "$*" >> '{log}'
case "$1 $2" in
  "pr create") echo "https://github.com/o/r/pull/7" ;;
  "pr view") echo '{{"number":42,"title":"Feature","url":"https://github.com/o/r/pull/42","state":"OPEN","isDraft":false,"baseRefName":"main","headRefName":"feat","headRefOid":"0123456789abcdef0123","isCrossRepository":false,"maintainerCanModify":true,"headRepository":{{"name":"r"}},"headRepositoryOwner":{{"login":"o"}}}}' ;;
esac"#,
            log = log.display()
        ),
    );
    GhOverride::install(&bin.join("gh"))
}

/// Fake `git`: strips the hardening `-c` pairs, records the subcommand, and answers from
/// `cases` (a shell `case` body over the joined argv). Unmatched calls exit 1.
fn fake_git(bin: &Path, log: &Path, cases: &str) {
    write_script(
        bin,
        "git",
        &format!(
            r#"while :; do case "$1" in -c) shift 2 ;; --no-pager) shift ;; *) break ;; esac; done
printf '%s\n' "$*" >> '{log}'
case "$*" in
{cases}
  *) exit 1 ;;
esac"#,
            log = log.display()
        ),
    );
}

fn read_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn pr_create_builds_gh_argv_with_body_file() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let log = tmp.path().join("gh.log");
    let _gh = fake_gh(&bin, &log);
    let _env = FakeEnv::install(&bin, tmp.path());

    let ws = Workspace::new(tmp.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let args = json!({
        "repo": "o/r",
        "title": " Feature ",
        "body": "Some body",
        "base": "main",
        "head": "feat",
        "draft": true,
        "reviewer": ["alice"],
        "assignee": ["bob"],
        "label": ["bug", "  "],
    });
    let out = run("pr_create", &ws, &args, &cancel).unwrap();

    let calls = read_lines(&log);
    let create = calls.iter().find(|l| l.starts_with("pr create")).unwrap();
    assert!(
        create.starts_with(
            "pr create --repo o/r --title Feature --base main --head feat --draft \
                 --reviewer alice --assignee bob --label bug --body-file "
        ),
        "{create}"
    );
    assert!(!create.contains("--fill"));
    assert!(!create.contains("--body "));
    let data = out.data.unwrap();
    assert_eq!(data["url"], "https://github.com/o/r/pull/7");
}

#[test]
fn pr_create_fill_omits_title_and_body_flags() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let log = tmp.path().join("gh.log");
    let _gh = fake_gh(&bin, &log);
    let _env = FakeEnv::install(&bin, tmp.path());

    let ws = Workspace::new(tmp.path()).unwrap();
    let cancel = AtomicBool::new(false);
    run("pr_create", &ws, &json!({"fill": true}), &cancel).unwrap();

    let calls = read_lines(&log);
    assert_eq!(calls.first().map(String::as_str), Some("pr create --fill"));
}

#[test]
fn pr_create_without_body_passes_empty_body() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let log = tmp.path().join("gh.log");
    let _gh = fake_gh(&bin, &log);
    let _env = FakeEnv::install(&bin, tmp.path());

    let ws = Workspace::new(tmp.path()).unwrap();
    let cancel = AtomicBool::new(false);
    run("pr_create", &ws, &json!({"title": "T"}), &cancel).unwrap();

    let calls = read_lines(&log);
    assert_eq!(
        calls.first().map(String::as_str),
        Some("pr create --title T --body ")
    );
}

#[test]
fn pr_push_adds_force_with_lease_only_when_requested() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let log = tmp.path().join("git.log");
    fake_git(
        &bin,
        &log,
        r#"  "symbolic-ref --quiet --short HEAD") echo pr-42 ;;
  "show-ref --verify --quiet refs/heads/pr-42") ;;
  "config --get branch.pr-42.dialPrHeadRef") echo feat ;;
  "config --get branch.pr-42.pushRemote") echo fork ;;
  "config --get remote.fork.url") echo https://github.com/x/r.git ;;
  "config --get branch.pr-42.dialPrUrl") echo https://github.com/o/r/pull/42 ;;
  "push "*) ;;"#,
    );
    let _env = FakeEnv::install(&bin, tmp.path());

    let ws = Workspace::new(tmp.path()).unwrap();
    let cancel = AtomicBool::new(false);
    run(
        "pr_push",
        &ws,
        &json!({"branch": "pr-42", "forceWithLease": true}),
        &cancel,
    )
    .unwrap();
    run("pr_push", &ws, &json!({"branch": "pr-42"}), &cancel).unwrap();

    let pushes: Vec<String> = read_lines(&log)
        .into_iter()
        .filter(|l| l.starts_with("push "))
        .collect();
    assert_eq!(pushes.len(), 2, "{pushes:?}");
    assert_eq!(
        pushes[0],
        "push --no-follow-tags --force-with-lease fork HEAD:refs/heads/feat"
    );
    assert_eq!(pushes[1], "push --no-follow-tags fork HEAD:refs/heads/feat");
    assert!(
        pushes
            .iter()
            .all(|l| !l.split_whitespace().any(|token| token == "--force")),
        "{pushes:?}"
    );
}

#[test]
fn pr_checkout_resolves_pr_number_and_creates_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir(&repo).unwrap();
    let gh_log = tmp.path().join("gh.log");
    let git_log = tmp.path().join("git.log");
    let _gh = fake_gh(&bin, &gh_log);
    fake_git(
        &bin,
        &git_log,
        &format!(
            r#"  "rev-parse --path-format=absolute --git-common-dir") echo '{}/.git' ;;
  "worktree list --porcelain") ;;
  "remote") echo origin ;;
  "config --get remote.origin.url") echo https://github.com/o/r.git ;;
  "fetch origin +refs/heads/feat:refs/remotes/origin/feat") ;;
  "branch pr-42 refs/remotes/origin/feat") ;;
  "worktree prune") ;;
  "config --get "*) exit 1 ;;
  "config "*) ;;
  "worktree add "*) mkdir -p "$3" ;;"#,
            repo.display()
        ),
    );
    let _env = FakeEnv::install(&bin, tmp.path());

    let ws = Workspace::new(&repo).unwrap();
    let cancel = AtomicBool::new(false);
    let out = run("pr_checkout", &ws, &json!({"pr": "42"}), &cancel).unwrap();

    let gh_calls = read_lines(&gh_log);
    assert!(
        gh_calls
            .iter()
            .any(|l| l.starts_with("pr view 42 --json baseRefName,")),
        "{gh_calls:?}"
    );
    let git_calls = read_lines(&git_log);
    assert!(
        git_calls
            .iter()
            .any(|l| l == "branch pr-42 refs/remotes/origin/feat"),
        "{git_calls:?}"
    );
    assert!(git_calls.iter().any(|l| l.starts_with("worktree add ")));
    assert!(out.text.starts_with("# Checked Out Pull Request #42"));
}

#[test]
fn fake_environment_is_scoped_to_pr_commands_not_other_git_consumers() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let log = tmp.path().join("git.log");
    fake_git(&bin, &log, r#"  "--version") echo scoped-fake ;;"#);
    let before = (std::env::var_os("PATH"), std::env::var_os("HOME"));
    let _env = FakeEnv::install(&bin, tmp.path());
    let ws = Workspace::new(tmp.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let output = Env {
        ws: &ws,
        cancel: &cancel,
    }
    .git(&["--version"], GIT_TIMEOUT)
    .unwrap();
    assert!(output.success);
    assert_eq!(output.stdout.trim(), "scoped-fake");
    assert_eq!(worktrees_root().unwrap(), tmp.path().join(".dial/wt"));
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let real = Command::new("git").arg("--version").output().unwrap();
                assert!(real.status.success());
                assert!(String::from_utf8_lossy(&real.stdout).starts_with("git version "));
                assert_eq!((std::env::var_os("PATH"), std::env::var_os("HOME")), before);
            })
            .join()
            .unwrap();
    });
}
