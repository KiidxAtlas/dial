use std::cell::RefCell;
#[cfg(unix)]
use std::fs;

use super::*;

thread_local! {
    static OVERRIDE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

pub(super) fn tea_override() -> Option<OsString> {
    OVERRIDE.with(|slot| slot.borrow().clone().map(PathBuf::into_os_string))
}

/// A fake `tea` for this thread that prints `stdout`/`stderr`, exits `status` and records argv.
/// It is a shell script, so the tests using it run on Unix only.
#[cfg(unix)]
struct FakeTea {
    dir: tempfile::TempDir,
}

#[cfg(unix)]
impl FakeTea {
    fn new(stdout: &str, stderr: &str, status: i32) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("stdout"), stdout).unwrap();
        fs::write(root.join("stderr"), stderr).unwrap();
        let script = format!(
            "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\0' \"$a\"; done > '{argv}'\ncat '{out}'\ncat '{err}' >&2\nexit {status}\n",
            argv = root.join("argv").display(),
            out = root.join("stdout").display(),
            err = root.join("stderr").display(),
        );
        let bin = root.join("tea");
        fs::write(&bin, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        OVERRIDE.with(|slot| *slot.borrow_mut() = Some(bin));
        Self { dir }
    }

    fn last_args(&self) -> Vec<String> {
        let raw = fs::read(self.dir.path().join("argv")).unwrap_or_default();
        raw.split(|b| *b == 0)
            .filter(|word| !word.is_empty())
            .map(|word| String::from_utf8_lossy(word).into_owned())
            .collect()
    }
}

#[cfg(unix)]
impl Drop for FakeTea {
    fn drop(&mut self) {
        OVERRIDE.with(|slot| *slot.borrow_mut() = None);
    }
}

fn run(args: Value) -> ToolOutput {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    execute(&ws, &args, &AtomicBool::new(false))
}

#[test]
fn registers_once_and_gates_writes() {
    let tools = tools(&Config::default());
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name(), "gitea_pr");
    let tool = GiteaPr;
    for op in OPS {
        let args = json!({"op": op});
        assert_eq!(tool.needs_approval(&args), !READ_OPS.contains(op), "{op}");
        assert!(!tool.may_modify_workspace(&args));
    }
    assert!(tool.needs_approval(&json!({})));
    assert_eq!(tool.schema()["function"]["name"], "gitea_pr");
}

#[cfg(unix)]
#[test]
fn pr_list_renders_json_rows_as_untrusted_data() {
    let fake = FakeTea::new(
        r#"[{"index":"7","state":"open","title":"Fix\nthing","author":"ana","head":"fix","base":"main","url":"https://git.example/o/r/pulls/7"}]"#,
        "",
        0,
    );
    let out = run(json!({"op":"pr_list","repo":"o/r","limit":5}));
    assert!(!out.is_error, "{}", out.text);
    assert!(
        out.text
            .contains("#7 [open] Fix thing (fix → main) by @ana https://git.example/o/r/pulls/7"),
        "{}",
        out.text
    );
    assert!(out.text.starts_with("[Untrusted Gitea pull requests"));
    assert_eq!(
        fake.last_args(),
        strings(&[
            "pulls",
            "list",
            "--output",
            "json",
            "--fields",
            LIST_FIELDS,
            "--state",
            "open",
            "--limit",
            "5",
            "--repo",
            "o/r"
        ])
    );
}

#[cfg(unix)]
#[test]
fn writes_pass_bodies_after_double_dash() {
    let fake = FakeTea::new("", "", 0);
    let out = run(json!({"op":"pr_comment","pr":"3","body":"--delete everything"}));
    assert_eq!(out.text, "Comment posted.");
    assert_eq!(
        fake.last_args(),
        strings(&["comment", "3", "--", "--delete everything"])
    );
    let out = run(json!({"op":"pr_merge","pr":3,"style":"squash"}));
    assert_eq!(out.text, "Pull request merged.");
    assert_eq!(
        fake.last_args(),
        strings(&["pulls", "merge", "3", "--style", "squash"])
    );
}

#[cfg(unix)]
#[test]
fn failures_map_to_next_steps() {
    let _fake = FakeTea::new("", "Error: No gitea login configured", 1);
    let out = run(json!({"op":"pr_view","pr":"1"}));
    assert!(out.is_error);
    assert!(out.text.contains("tea login add"), "{}", out.text);
}

#[test]
fn arguments_that_could_read_as_flags_are_refused() {
    for repo in ["-x/r", "o/../r", "o", "o/r/x", "../r"] {
        let out = run(json!({"op":"pr_list","repo":repo}));
        assert!(out.is_error && out.text.contains("owner/repo"), "{repo}");
    }
    for pr in ["-1", "0", "1a", ""] {
        let out = run(json!({"op":"pr_view","pr":pr}));
        assert!(
            out.is_error && out.text.contains("pull request number"),
            "{pr:?}"
        );
    }
    let out = run(json!({"op":"pr_merge","pr":"1","style":"fast"}));
    assert!(out.text.contains("style must be one of"));
}
