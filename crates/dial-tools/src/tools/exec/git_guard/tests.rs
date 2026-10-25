use super::*;

fn judge(command: &str) -> Option<Danger> {
    let root = Path::new("/work/project");
    find(command, root, &[root])
}

fn refused(command: &str) -> String {
    judge(command)
        .unwrap_or_else(|| panic!("`{command}` should be refused"))
        .form
}

fn allowed(command: &str) {
    assert_eq!(judge(command), None, "`{command}` should run");
}

#[test]
fn destructive_forms_are_refused() {
    for command in [
        "git reset --hard",
        "git reset --hard HEAD~1",
        "git reset HEAD~1 --hard",
        "git reset --har",
        "git stash",
        "git stash push -m wip",
        "git stash save wip",
        "git stash drop",
        "git stash clear",
        "git stash pop",
        "git checkout -- src/lib.rs",
        "git checkout .",
        "git checkout -f",
        "git checkout -f main",
        "git checkout HEAD -- src",
        "git restore src/lib.rs",
        "git restore .",
        "git restore --worktree --staged src",
        "git restore -SW src",
        "git clean -f",
        "git clean -fd",
        "git clean -xdf",
        "git clean --force",
        "git switch --discard-changes main",
        "git switch -f main",
    ] {
        refused(command);
    }
}

#[test]
fn harmless_forms_run() {
    for command in [
        "git status",
        "git reset",
        "git reset --soft HEAD~1",
        "git reset --mixed HEAD~1",
        "git reset src/lib.rs",
        "git stash list",
        "git stash show -p",
        "git checkout -b new",
        "git checkout -B new origin/main",
        "git checkout main",
        "git checkout --ours -- conflicted.rs",
        "git restore --staged src/lib.rs",
        "git restore -S src/lib.rs",
        "git clean -n",
        "git clean -nfd",
        "git clean --dry-run -f",
        "git switch main",
        "git switch -c topic",
        "git commit -m 'git reset --hard'",
        "echo git reset --hard",
        "git log --oneline",
        "git diff",
    ] {
        allowed(command);
    }
}

#[test]
fn global_options_do_not_hide_the_subcommand() {
    assert_eq!(refused("git -c x=y reset --hard"), "git reset --hard");
    assert_eq!(
        refused("git --no-pager -c core.pager=cat stash"),
        "git stash"
    );
    assert_eq!(refused("git -C . reset --hard"), "git reset --hard");
    allowed("git -c x=y reset --soft");
    allowed("git -c reset.hard=1 status");
}

#[test]
fn quoting_and_wrappers_are_read() {
    refused("git 'reset' \"--hard\"");
    refused("git reset --hard\\");
    refused("env GIT_PAGER=cat git reset --hard");
    refused("command git stash");
    refused("FOO=1 git clean -f");
    refused("sh -c 'git reset --hard'");
    refused("bash -lc \"cd sub && git stash\"");
    refused("(git stash)");
    refused("if true; then git clean -fd; fi");
    refused("git status; git reset --hard");
    refused("git fetch && git checkout .");
    refused("true | git stash");
    allowed("git commit -m \"don't git reset --hard\"");
    allowed("echo 'git stash' | cat");
    allowed("# git reset --hard\ngit status");
    allowed("cat <<EOF\ngit reset --hard\nEOF");
}

#[test]
fn unreadable_or_dynamic_scripts_are_not_guessed() {
    allowed("git reset --hard 'unterminated");
    allowed("$GIT reset --hard");
    allowed("git $SUB --hard");
    allowed("sh -c \"$SCRIPT\"");
    // A quoted dynamic argument never makes a harmless command look destructive either.
    allowed("git restore --staged \"$FILE\"");
}

#[test]
fn directories_follow_dash_c_and_cd() {
    // Another repository outside the workspace is not guarded.
    allowed("git -C /somewhere/else reset --hard");
    allowed("cd /somewhere/else && git stash");
    allowed("git -C ../other clean -f");
    // The workspace is.
    refused("git -C /work/project/sub reset --hard");
    refused("git -C sub reset --hard");
    refused("cd sub && git stash");
    refused("cd /somewhere/else && cd /work/project && git stash");
    // A cd in a subshell or pipeline does not leave the directory for later commands.
    refused("(cd /somewhere/else) && git stash");
    refused("cd /somewhere/else | cat; git stash");
    // An unknown directory counts as inside.
    refused("cd \"$DIR\" && git stash");
    refused("git --git-dir=/x/.git reset --hard");
    refused("git -C \"$DIR\" stash");
}

#[test]
fn checkout_of_an_existing_path_is_a_restore() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("notes.txt"), "x").unwrap();
    let judged = |command: &str| find(command, &root, &[root.as_path()]);
    assert!(judged("git checkout notes.txt").is_some());
    assert!(judged("git checkout no-such-branch-or-file").is_none());
    assert!(judged("git checkout -b notes.txt").is_none());
}

#[test]
fn the_message_names_the_form_and_the_way_forward() {
    let message = judge("git reset --hard").unwrap().message(false);
    assert!(
        message.contains("`git reset --hard` was not run"),
        "{message}"
    );
    assert!(message.contains("shell.git_guard"), "{message}");
    assert!(message.contains("isolation"), "{message}");
    assert!(message.contains("git reset --soft"), "{message}");
}

#[test]
fn policy_decides_where_the_guard_applies() {
    use crate::config::types::GitGuard;
    let dir = tempfile::tempdir().unwrap();
    let base = Workspace::new(dir.path()).unwrap();
    let root = base.root().to_path_buf();
    let check = |ws: &Workspace| guard(ws, "git reset --hard", &root).is_some();
    // The default guards only workers on the shared checkout.
    assert!(!check(&base));
    assert!(check(&base.clone().with_shared_checkout(true)));
    let always = crate::config::types::ShellConfig {
        git_guard: GitGuard::Always,
        ..Default::default()
    };
    assert!(check(&base.clone().with_shell_config(&always)));
    let off = crate::config::types::ShellConfig {
        git_guard: GitGuard::Off,
        ..Default::default()
    };
    assert!(!check(
        &base
            .clone()
            .with_shell_config(&off)
            .with_shared_checkout(true)
    ));
    // A rebased copy (an isolated worker) is not on the shared checkout.
    let copy = base
        .clone()
        .with_shared_checkout(true)
        .rebased(dir.path())
        .unwrap();
    assert!(!check(&copy));
    let refusal = guard(&base.with_shared_checkout(true), "git stash", &root).unwrap();
    assert!(refusal.is_error);
    assert_eq!(ToolStatus::from_output(&refusal), ToolStatus::Blocked);
}
