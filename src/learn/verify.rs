//! Deterministic, declarative task verification. A `Check` looks at the final workspace and the
//! agent's final answer and returns pass/fail: no model is involved, which is what makes outcomes
//! produced with it externally grounded (for gate cases, self-play and training data alike).
use crate::tools::{self, Workspace, sandbox::Policy};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Component, Path, PathBuf},
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
    let Ok(ws) = Workspace::new(workspace) else {
        return false;
    };
    let ws = ws.with_policy(Policy::Strict {
        allow_network: false,
    });
    let out = tools::execute(&ws, "bash", &json!({"command": command, "timeout": 30}), 30);
    // bash output ends with "[exit code: N]" (see `tools::shell::bash_tool`).
    let Some(at) = out.text.rfind("[exit code: ") else {
        return false;
    };
    let status: Option<i32> = out.text[at + 12..]
        .trim_end()
        .trim_end_matches(']')
        .parse()
        .ok();
    let stdout = out.text[..at].trim();
    status == Some(expect_exit) && expect_stdout.is_none_or(|e| stdout == e.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

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
            command: "python3 t.py".into(),
            expect_stdout: Some("42".into()),
            expect_exit: 0,
        };
        assert!(ok.verify(dir.path(), ""));
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
    }
}
