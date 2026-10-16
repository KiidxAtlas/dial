//! Deterministic, declarative task verification. A `Check` looks at the final workspace and the
//! agent's final answer and returns pass/fail: no model is involved, which is what makes outcomes
//! produced with it externally grounded (for gate cases, self-play and training data alike).
use crate::runtime::{execution::ProcessOutcome, process};
use crate::tools::sandbox::Sandbox;
#[cfg(test)]
use crate::tools::{self, Workspace, sandbox::Policy};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
use std::{
    fs,
    path::{Component, Path, PathBuf},
    sync::atomic::AtomicBool,
    time::Duration,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Check {
    /// Operator-owned checks restored after execution, never taken from solver edits.
    Repository {
        files: std::collections::BTreeMap<String, String>,
        command: String,
    },
    /// Final answer equals `expected` after trimming whitespace.
    AnswerExact {
        expected: String,
    },
    AnswerContains {
        needle: String,
    },
    /// Final answer parses as JSON equal to `expected`.
    AnswerJson {
        expected: Value,
    },
    /// File content equals `content` ignoring trailing whitespace.
    FileEquals {
        path: String,
        content: String,
    },
    FileContains {
        path: String,
        needle: String,
    },
    FileAbsent {
        path: String,
    },
    /// JSON file whose value at JSON-pointer `pointer` equals `expected`.
    JsonFile {
        path: String,
        pointer: String,
        expected: Value,
    },
    /// Run `command` with bash in the workspace under the strict sandbox; stdout (trimmed) must equal
    /// `expect_stdout` when given and the exit status must be `expect_exit`.
    Command {
        command: String,
        #[serde(default)]
        expect_stdout: Option<String>,
        #[serde(default)]
        expect_exit: i32,
    },
    /// Every nested check must pass.
    All {
        checks: Vec<Check>,
    },
}

impl Check {
    pub fn verify(&self, workspace: &Path, answer: &str) -> bool {
        match self {
            Check::Repository { files, command } => {
                super::repository::verify(workspace, files, command)
            }
            Check::AnswerExact { expected } => answer.trim() == expected.trim(),
            Check::AnswerContains { needle } => answer.contains(needle.as_str()),
            Check::AnswerJson { expected } => {
                serde_json::from_str::<Value>(answer.trim()).is_ok_and(|v| &v == expected)
            }
            Check::FileEquals { path, content } => {
                read(workspace, path).is_some_and(|c| c.trim_end() == content.trim_end())
            }
            Check::FileContains { path, needle } => {
                read(workspace, path).is_some_and(|c| c.contains(needle.as_str()))
            }
            Check::FileAbsent { path } => match confine(workspace, path) {
                Some(p) => fs::symlink_metadata(p).is_err(),
                None => false,
            },
            Check::JsonFile {
                path,
                pointer,
                expected,
            } => read(workspace, path)
                .and_then(|c| serde_json::from_str::<Value>(&c).ok())
                .is_some_and(|v| v.pointer(pointer) == Some(expected)),
            Check::Command {
                command,
                expect_stdout,
                expect_exit,
            } => run_command(workspace, command, expect_stdout.as_deref(), *expect_exit),
            Check::All { checks } => {
                !checks.is_empty() && checks.iter().all(|c| c.verify(workspace, answer))
            }
        }
    }
}

/// A path under `root` with no `..`, no absolute prefix and no symlinked final component.
fn confine(root: &Path, rel: &str) -> Option<PathBuf> {
    let p = Path::new(rel);
    if rel.is_empty()
        || rel.contains('\0')
        || !p.components().all(|c| matches!(c, Component::Normal(_)))
    {
        return None;
    }
    let full = root.join(p);
    // A symlink anywhere along the way could point outside the workspace.
    let mut probe = root.to_path_buf();
    for part in p.components() {
        probe.push(part);
        if fs::symlink_metadata(&probe).is_ok_and(|m| m.file_type().is_symlink()) {
            return None;
        }
    }
    Some(full)
}

fn read(root: &Path, rel: &str) -> Option<String> {
    let path = confine(root, rel)?;
    if fs::metadata(&path).ok()?.len() > 4 * 1024 * 1024 {
        return None;
    }
    fs::read_to_string(path).ok()
}

fn run_command(
    workspace: &Path,
    command: &str,
    expect_stdout: Option<&str>,
    expect_exit: i32,
) -> bool {
    let Ok(root) = workspace.canonicalize() else {
        return false;
    };
    // Preserve Workspace's admission boundary even though verification needs raw process streams.
    if !root.is_dir() || root.parent().is_none() {
        return false;
    }
    // Keep the strict offline sandbox, but capture raw streams separately. Shell tool text merges
    // stderr and adds status/artifact diagnostics; none of that is command stdout or authority.
    let Ok(mut child) = Sandbox::detect().command(
        &root,
        false,
        "/bin/bash",
        &["--noprofile", "--norc", "-c", command],
    ) else {
        return false;
    };
    let scratch = root.join(crate::tools::SCRATCH_DIR);
    if fs::create_dir_all(&scratch).is_err() {
        return false;
    }
    child
        .current_dir(&root)
        .env_clear()
        .envs(std::env::vars().filter(|(key, value)| {
            !crate::tools::sandbox::is_secret_env(key, value)
                && !matches!(key.as_str(), "BASH_ENV" | "ENV" | "SHELLOPTS" | "BASHOPTS")
        }))
        .env("TMPDIR", &scratch);
    let mut policy = process::Policy::bounded(Duration::from_secs(30), 4 * 1024 * 1024);
    policy.fail_on_output_limit = true;
    let Ok(out) =
        process::run_with_policy(child, None, policy, &AtomicBool::new(false), &mut |_, _| {})
    else {
        return false;
    };
    command_matches(&out.outcome(), &out.stdout, expect_stdout, expect_exit)
}

fn command_matches(
    outcome: &ProcessOutcome,
    stdout: &[u8],
    expect_stdout: Option<&str>,
    expect_exit: i32,
) -> bool {
    outcome.started
        && outcome.output_complete
        && !outcome.timed_out
        && !outcome.cancelled
        && outcome.signal.is_none()
        && outcome.exit_code == Some(expect_exit)
        && expect_stdout.is_none_or(|expected| {
            std::str::from_utf8(stdout).is_ok_and(|actual| actual.trim() == expected.trim())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_verdict_uses_typed_completion_and_raw_stdout() {
        let complete = ProcessOutcome {
            started: true,
            exit_code: Some(3),
            signal: None,
            timed_out: false,
            cancelled: false,
            output_complete: true,
        };
        assert!(command_matches(&complete, b"42\n", Some("42"), 3));
        assert!(command_matches(&complete, b"", Some(""), 3));
        assert!(!command_matches(&complete, b"[exit code: 0]\n", None, 0));
        assert!(!command_matches(
            &complete,
            b"42\n[exit code: 3]\n",
            Some("42"),
            3
        ));
        for incomplete in [
            ProcessOutcome {
                started: false,
                ..complete.clone()
            },
            ProcessOutcome {
                cancelled: true,
                ..complete.clone()
            },
            ProcessOutcome {
                timed_out: true,
                ..complete.clone()
            },
            ProcessOutcome {
                output_complete: false,
                ..complete.clone()
            },
            ProcessOutcome {
                signal: Some(9),
                ..complete.clone()
            },
            ProcessOutcome {
                exit_code: None,
                ..complete.clone()
            },
        ] {
            assert!(!command_matches(&incomplete, b"42", Some("42"), 3));
        }
    }

    #[test]
    fn checks_judge_final_state_not_intent() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
        fs::write(dir.path().join("c.json"), "{\"k\":{\"n\":7}}").unwrap();
        let w = dir.path();
        assert!(
            Check::FileEquals {
                path: "a.txt".into(),
                content: "hello".into()
            }
            .verify(w, "")
        );
        assert!(
            !Check::FileEquals {
                path: "a.txt".into(),
                content: "bye".into()
            }
            .verify(w, "")
        );
        assert!(
            Check::JsonFile {
                path: "c.json".into(),
                pointer: "/k/n".into(),
                expected: json!(7)
            }
            .verify(w, "")
        );
        assert!(
            Check::FileAbsent {
                path: "nope".into()
            }
            .verify(w, "")
        );
        assert!(
            Check::AnswerExact {
                expected: "x".into()
            }
            .verify(w, " x \n")
        );
        assert!(
            Check::AnswerJson {
                expected: json!([1, 2])
            }
            .verify(w, "[1,2]")
        );
        assert!(
            !Check::All { checks: vec![] }.verify(w, ""),
            "an empty All must not pass vacuously"
        );
    }

    #[cfg(unix)]
    #[test]
    fn paths_cannot_escape_or_follow_symlinks() {
        let outer = tempfile::tempdir().unwrap();
        let dir = outer.path().join("w");
        fs::create_dir(&dir).unwrap();
        fs::write(outer.path().join("secret"), "s").unwrap();
        std::os::unix::fs::symlink(outer.path().join("secret"), dir.join("link")).unwrap();
        assert!(
            !Check::FileContains {
                path: "../secret".into(),
                needle: "s".into()
            }
            .verify(&dir, "")
        );
        assert!(
            !Check::FileContains {
                path: "link".into(),
                needle: "s".into()
            }
            .verify(&dir, "")
        );
        assert!(
            !Check::FileContains {
                path: "/etc/hosts".into(),
                needle: "".into()
            }
            .verify(&dir, "")
        );
    }

    #[test]
    fn command_check_runs_in_the_sandbox() {
        if !crate::tools::sandbox::require_for_test() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("t.py"), "print(6*7)\n").unwrap();
        let ok = Check::Command {
            command: "python3 t.py; printf 'trailing diagnostic [exit code: 99]\\n' >&2".into(),
            expect_stdout: Some("42".into()),
            expect_exit: 0,
        };
        assert!(
            ok.verify(dir.path(), ""),
            "raw sandbox command output: {}",
            {
                let ws = Workspace::new(dir.path())
                    .unwrap()
                    .with_policy(Policy::Strict {
                        allow_network: false,
                        deny_read: Vec::new(),
                        extra_write: Vec::new(),
                    });
                tools::execute(
                    &ws,
                    "bash",
                    &json!({"command":"python3 t.py","timeout":30}),
                    30,
                )
                .text
            }
        );
        assert!(
            !Check::Command {
                command: "python3 t.py".into(),
                expect_stdout: Some("41".into()),
                expect_exit: 0
            }
            .verify(dir.path(), "")
        );
        assert!(
            Check::Command {
                command: "exit 3".into(),
                expect_stdout: None,
                expect_exit: 3
            }
            .verify(dir.path(), "")
        );
        // A program can print convincing status/timeout text; only actual completion is proof.
        assert!(
            Check::Command {
                command: "printf '[exit code: 99]\\n'; exit 3".into(),
                expect_stdout: Some("[exit code: 99]".into()),
                expect_exit: 3,
            }
            .verify(dir.path(), "")
        );
        assert!(
            !Check::Command {
                command: "printf '[exit code: 0]\\n'; exit 3".into(),
                expect_stdout: None,
                expect_exit: 0,
            }
            .verify(dir.path(), "")
        );
        assert!(
            !Check::Command {
                command: "head -c 4194305 /dev/zero".into(),
                expect_stdout: None,
                expect_exit: 0,
            }
            .verify(dir.path(), "")
        );
        assert!(
            !Check::Command {
                command: "kill -TERM $$".into(),
                expect_stdout: None,
                expect_exit: 0,
            }
            .verify(dir.path(), "")
        );
    }
}
