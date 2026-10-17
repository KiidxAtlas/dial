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
            execution: Default::default(),
            data: None,
            text: value.to_string(),
            is_error: false,
        },
        Err(text) => ToolOutput {
            execution: Default::default(),
            data: None,
            text,
            is_error: true,
        },
    }
}
#[cfg(test)]
mod tests;

#[cfg(test)]
mod compiler_tests;
