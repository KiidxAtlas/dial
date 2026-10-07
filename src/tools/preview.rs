//! Bounded disposable source overlays. Preview results never certify the live workspace.
use super::{ToolOutput, Workspace};
use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
pub fn schema() -> Value {
    json!({"type":"function","function":{"name":"edit_preview","description":"Parse proposed complete source before writing. check=type runs an offline compiler in a bounded disposable project copy (may execute project build scripts, normal approval required). No live files change; this is not behavioral verification.","parameters":{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string","maxLength":1048576},"check":{"type":"string","enum":["syntax","type"]}},"required":["path","content"],"additionalProperties":false}}})
}
pub fn run(ws: &Workspace, args: &Value, cancel: &AtomicBool) -> ToolOutput {
    let result = (|| -> Result<Value, String> {
        let path = args["path"].as_str().ok_or("path is required")?;
        let content = args["content"].as_str().ok_or("content is required")?;
        if content.len() > 1024 * 1024 {
            return Err("proposed source exceeds 1 MiB budget".into());
        }
        if !crate::storage::experience::safe_path(path) {
            return Err("preview requires a relative public source path".into());
        }
        let target = ws.resolve(path)?;
        let syntax = super::syntax::report(&target, content);
        if args["check"].as_str().unwrap_or("syntax") == "syntax" {
            return Ok(
                json!({"syntax":syntax,"scope":"proposed source only; no type or behavioral verdict"}),
            );
        }
        if args["check"] != "type" {
            return Err("check must be syntax or type".into());
        }
        if syntax["valid"] == false {
            return Ok(json!({"syntax":syntax,"type_check":"skipped: fix parse errors first"}));
        }
        if !path.ends_with(".rs") || !ws.root().join("Cargo.toml").is_file() {
            return Ok(
                json!({"syntax":syntax,"type_check":"unavailable: disposable type preview currently supports Cargo/Rust projects"}),
            );
        }
        let temp = tempfile::tempdir().map_err(|e| e.to_string())?;
        let start = Instant::now();
        let mut count = 0usize;
        let mut bytes = 0u64;
        let mut failure = None;
        let mut walk = super::walk::Walk::new(ws);
        walk.run(ws.root(), &mut |source| {
            if cancel.load(Ordering::SeqCst) || start.elapsed() > Duration::from_secs(3) {
                failure = Some("preview copy cancelled or exceeded time budget".to_string());
                return false;
            }
            let Ok(relative) = source.strip_prefix(ws.root()) else {
                return true;
            };
            let relative = relative.to_string_lossy();
            if !crate::storage::experience::safe_path(&relative) {
                return true;
            }
            let Ok(meta) = fs::symlink_metadata(source) else {
                failure = Some("preview source disappeared".into());
                return false;
            };
            if meta.file_type().is_symlink() {
                failure = Some("preview cannot faithfully copy project symlinks".into());
                return false;
            }
            if !meta.is_file() {
                return true;
            }
            count += 1;
            bytes += meta.len();
            if count > 2048 || bytes > 32 * 1024 * 1024 {
                failure = Some("preview project exceeds 2048 files / 32 MiB budget".into());
                return false;
            }
            let dest = temp.path().join(Path::new(relative.as_ref()));
            let copied = (|| -> std::io::Result<()> {
                fs::create_dir_all(dest.parent().unwrap())?;
                let mut data = Vec::new();
                fs::File::open(source)?
                    .take(meta.len() + 1)
                    .read_to_end(&mut data)?;
                if data.len() as u64 != meta.len() {
                    return Err(std::io::Error::other("preview source changed during copy"));
                }
                fs::write(dest, data)?;
                Ok(())
            })();
            if let Err(e) = copied {
                failure = Some(e.to_string());
                return false;
            }
            true
        });
        if let Some(failure) = failure {
            return Err(failure);
        }
        if walk.truncated {
            return Err("preview source walk was incomplete".into());
        }
        let dest = temp.path().join(path);
        fs::create_dir_all(dest.parent().unwrap()).map_err(|e| e.to_string())?;
        fs::write(&dest, content).map_err(|e| e.to_string())?;
        let overlay = ws.rebased(temp.path()).map_err(|e| e.to_string())?;
        // Keep compiler artifacts inside the overlay even when the user exports CARGO_TARGET_DIR.
        let target = overlay
            .root()
            .join("target")
            .to_string_lossy()
            .replace('\'', "'\"'\"'");
        let command =
            format!("CARGO_TARGET_DIR='{target}' cargo check --offline --all-targets --quiet -j1");
        let output = super::shell::bash_tool_streaming(
            &overlay,
            &json!({"command":command}),
            30,
            &|_| {},
            cancel,
        )?;
        Ok(
            json!({"syntax":syntax,"compiler_output":output.text,"compiler_failed":output.is_error,"scope":"disposable overlay; offline type check, not live workspace or behavioral verification","copied_files":count}),
        )
    })();
    match result {
        Ok(value) => ToolOutput {
            data: None,
            text: value.to_string(),
            is_error: false,
        },
        Err(text) => ToolOutput {
            data: None,
            text,
            is_error: true,
        },
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn syntax_preview_never_changes_live_source() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        let out = run(
            &ws,
            &json!({"path":"a.rs","content":"fn a( {","check":"syntax"}),
            &AtomicBool::new(false),
        );
        assert!(!out.is_error);
        assert_eq!(
            serde_json::from_str::<Value>(&out.text).unwrap()["syntax"]["valid"],
            false
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("a.rs")).unwrap(),
            "fn a() {}"
        );
    }
    #[test]
    fn private_and_escaping_paths_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        for path in ["../a.rs", ".env", "private/key.rs"] {
            assert!(
                run(
                    &ws,
                    &json!({"path":path,"content":""}),
                    &AtomicBool::new(false)
                )
                .is_error
            );
        }
    }
}

#[cfg(test)]
mod compiler_tests {
    use super::*;
    #[test]
    fn compiler_preview_reports_type_failure_and_preserves_project() {
        if !crate::tools::sandbox::require_for_test() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("src")).unwrap();
        fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname='preview_fixture'\nversion='0.0.0'\nedition='2024'\n",
        )
        .unwrap();
        fs::write(
            root.path().join("src/lib.rs"),
            "pub fn value() -> u8 { 1 }\n",
        )
        .unwrap();
        // Compiler previews need the operator's installed toolchain. Give only this
        // disposable test an explicit Dev policy and its Cargo/Rustup directories;
        // production previews retain their workspace's configured sandbox policy.
        let home = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap();
        let mut toolchains = Vec::new();
        for (key, fallback) in [("CARGO_HOME", ".cargo"), ("RUSTUP_HOME", ".rustup")] {
            let path = std::env::var_os(key)
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| home.join(fallback));
            if path.is_dir() {
                toolchains.push(path);
            }
        }
        let ws =
            Workspace::new(root.path())
                .unwrap()
                .with_policy(super::super::sandbox::Policy::Dev {
                    deny_read: vec![],
                    extra_write: toolchains,
                    allow_network: false,
                });
        let output = run(
            &ws,
            &json!({"path":"src/lib.rs","content":"pub fn value() -> u8 { \"bad\" }\n","check":"type"}),
            &AtomicBool::new(false),
        );
        assert!(!output.is_error, "{}", output.text);
        let result: Value = serde_json::from_str(&output.text).unwrap();
        assert_eq!(result["compiler_failed"], true);
        assert!(
            result["compiler_output"]
                .as_str()
                .unwrap()
                .contains("mismatched types"),
            "{}",
            output.text
        );
        assert_eq!(
            fs::read_to_string(root.path().join("src/lib.rs")).unwrap(),
            "pub fn value() -> u8 { 1 }\n"
        );
        assert!(!root.path().join("target").exists());
        assert!(!root.path().join("Cargo.lock").exists());
    }
}
