use super::testkit::*;
use crate::tools::{ToolStatus, surface::outcome::Effects};
use serde_json::json;

#[test]
fn absolute_paths_resolve_under_added_roots_and_relative_under_primary() {
    let (_dir, mut ws) = workspace();
    let extra = tempfile::tempdir().unwrap();
    std::fs::write(extra.path().join("e.txt"), "e").unwrap();
    assert!(ws.add_root(extra.path()).unwrap());

    let inside = extra.path().join("e.txt");
    assert_eq!(
        ws.resolve(inside.to_str().unwrap()).unwrap(),
        inside.canonicalize().unwrap()
    );
    assert_eq!(ws.resolve("p.txt").unwrap(), ws.root().join("p.txt"));
    assert_eq!(ws.roots().count(), 2);
    assert_eq!(
        ws.additional_roots(),
        [extra.path().canonicalize().unwrap()]
    );
}

#[test]
fn paths_outside_every_root_are_refused() {
    let (_dir, mut ws) = workspace();
    let extra = tempfile::tempdir().unwrap();
    ws.add_root(extra.path()).unwrap();
    let other = tempfile::tempdir().unwrap();
    std::fs::write(other.path().join("o.txt"), "o").unwrap();
    let err = ws
        .resolve(other.path().join("o.txt").to_str().unwrap())
        .unwrap_err();
    assert!(err.contains("outside the workspace"), "{err}");
    let escape = format!("{}/../{}", extra.path().display(), "o.txt");
    assert!(ws.resolve(&escape).is_err());
}

#[test]
fn removed_root_stops_resolving_and_primary_cannot_be_added_or_removed() {
    let (_dir, mut ws) = workspace();
    let extra = tempfile::tempdir().unwrap();
    std::fs::write(extra.path().join("e.txt"), "e").unwrap();
    let file = extra.path().join("e.txt");
    assert!(ws.add_root(extra.path()).unwrap());
    assert!(!ws.add_root(extra.path()).unwrap(), "second add is a no-op");
    assert!(ws.remove_root(extra.path()).unwrap());
    assert!(!ws.remove_root(extra.path()).unwrap());
    assert!(ws.resolve(file.to_str().unwrap()).is_err());

    let root = ws.root().to_path_buf();
    assert!(ws.add_root(&root).is_err());
    assert!(ws.remove_root(&root).is_err());
}

#[test]
fn with_additional_roots_rejects_missing_directories() {
    let (_dir, ws) = workspace();
    let missing = ws.root().join("does-not-exist");
    assert!(ws.with_additional_roots(&[missing]).is_err());
}

/// `/add-dir` must make file tools work under the added directory, not just path resolution:
/// the confined edit transaction used to walk only from the primary root.
#[test]
fn file_tools_read_and_write_under_an_added_root() {
    let (_dir, mut ws) = workspace();
    let extra = tempfile::tempdir().unwrap();
    std::fs::write(extra.path().join("e.txt"), "existing\n").unwrap();
    ws.add_root(extra.path()).unwrap();
    let root = extra.path().canonicalize().unwrap();

    let read = run(&ws, "read", json!({"path": root.join("e.txt")}));
    assert!(!read.is_error, "{}", read.text);
    assert!(read.text.contains("existing"), "{}", read.text);

    let created = root.join("sub/new.txt");
    let write = run(&ws, "write", json!({"path": created, "content": "made\n"}));
    assert!(!write.is_error, "{}", write.text);
    assert_eq!(std::fs::read_to_string(&created).unwrap(), "made\n");
}

/// An absolute path outside every root is a structural boundary refusal: `Blocked` with no
/// effects, so recovery names the permission boundary instead of a malformed call.
#[test]
fn write_outside_every_root_is_blocked_with_no_effects() {
    let (_dir, mut ws) = workspace();
    let other = tempfile::tempdir().unwrap();
    let target = other.path().join("out.txt");
    let output = run(&ws, "write", json!({"path": target, "content": "x"}));
    assert!(output.is_error, "{}", output.text);
    assert_eq!(
        output.execution.status,
        Some(ToolStatus::Blocked),
        "{}",
        output.text
    );
    assert_eq!(output.execution.effects, Effects::None);
    assert!(output.text.contains("request_directory"), "{}", output.text);
    assert!(!target.exists());
    ws.add_root(other.path()).unwrap();
    let output = run(&ws, "write", json!({"path": target, "content": "x"}));
    assert!(!output.is_error, "{}", output.text);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "x");
}

/// Relative escapes and symlink escapes use the same resolver classification as absolute paths.
#[test]
fn relative_write_escape_is_blocked_and_malformed_outside_path_is_not() {
    let (_dir, ws) = workspace();
    let output = run(&ws, "write", json!({"path":"../denied.txt", "content":"x"}));
    assert_eq!(
        output.execution.status,
        Some(ToolStatus::Blocked),
        "{}",
        output.text
    );
    assert_eq!(output.execution.effects, Effects::None);
    let malformed = format!("{}/a\0b", ws.root().parent().unwrap().display());
    let output = run(&ws, "write", json!({"path":malformed, "content":"x"}));
    assert_ne!(
        output.execution.status,
        Some(ToolStatus::Blocked),
        "{}",
        output.text
    );
}

#[cfg(unix)]
#[test]
fn symlink_write_escape_is_blocked_with_no_effects() {
    let (_dir, ws) = workspace();
    let other = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(other.path(), ws.root().join("link")).unwrap();
    let output = run(&ws, "write", json!({"path":"link/out.txt", "content":"x"}));
    assert_eq!(
        output.execution.status,
        Some(ToolStatus::Blocked),
        "{}",
        output.text
    );
    assert_eq!(output.execution.effects, Effects::None);
    assert!(!other.path().join("out.txt").exists());
}

/// A malformed path inside the workspace stays an ordinary error, not a boundary refusal.
#[test]
fn in_root_malformed_write_path_stays_an_error_not_blocked() {
    let (_dir, ws) = workspace();
    let malformed = ws.root().join("a\0b").display().to_string();
    let output = run(&ws, "write", json!({"path": malformed, "content": "x"}));
    assert!(output.is_error, "{}", output.text);
    assert_ne!(
        output.execution.status,
        Some(ToolStatus::Blocked),
        "{}",
        output.text
    );
}

#[test]
fn protected_write_is_blocked_with_no_effects() {
    let (_dir, ws) = workspace();
    let protected = ws.root().join("private");
    std::fs::create_dir(&protected).unwrap();
    let ws = ws.with_protected_paths(std::slice::from_ref(&protected));
    let output = run(
        &ws,
        "write",
        json!({"path":"private/out.txt", "content":"x"}),
    );
    assert_eq!(
        output.execution.status,
        Some(ToolStatus::Blocked),
        "{}",
        output.text
    );
    assert_eq!(output.execution.effects, Effects::None);
    assert!(!protected.join("out.txt").exists());
}

#[test]
fn read_only_roots_allow_native_reads_and_searches_but_not_mutations() {
    let (_dir, ws) = workspace();
    let extra = tempfile::tempdir().unwrap();
    let root = extra.path().canonicalize().unwrap();
    std::fs::write(root.join("e.txt"), "read-only-canary\n").unwrap();
    assert!(
        ws.resolve_read(root.join("e.txt").to_str().unwrap())
            .is_err()
    );
    let ws = ws
        .with_read_only_roots(std::slice::from_ref(&root))
        .unwrap();
    for (tool, args) in [
        ("read", json!({"path": root.join("e.txt")})),
        (
            "read",
            json!({"path": format!("{}:1-1", root.join("e.txt").display())}),
        ),
        ("glob", json!({"path": root, "pattern": "*.txt"})),
        ("grep", json!({"path": root, "pattern": "read-only-canary"})),
    ] {
        let output = run(&ws, tool, args);
        assert!(
            !output.is_error
                && output.text.contains(if tool == "read" {
                    "read-only-canary"
                } else {
                    "e.txt"
                }),
            "{tool}: {}",
            output.text
        );
    }
    let handle = ws
        .file_handle(root.join("e.txt").to_str().unwrap())
        .unwrap();
    assert!(!run(&ws, "read", json!({"path": handle})).is_error);
    for (tool, args) in [
        (
            "write",
            json!({"path": root.join("e.txt"), "content": "changed"}),
        ),
        (
            "write",
            json!({"path": root.join("new/created.txt"), "content": "changed"}),
        ),
        ("write", json!({"path": handle, "content": "changed"})),
        (
            "edit",
            json!({"path": root.join("e.txt"), "old_string": "read-only-canary", "new_string": "changed"}),
        ),
    ] {
        let output = if tool == "edit" {
            run(
                &ws.clone()
                    .with_edit_mode(crate::config::types::EditMode::Replace),
                tool,
                args,
            )
        } else {
            run(&ws, tool, args)
        };
        assert!(output.is_error, "{tool}: {}", output.text);
        assert_eq!(
            output.execution.effects,
            Effects::None,
            "{tool}: {}",
            output.text
        );
    }
    assert!(ws.resolve(root.join("e.txt").to_str().unwrap()).is_err());
    assert!(ws.observe_write(&root.join("e.txt")).is_err());
    assert!(ws.observe_delete(&root.join("e.txt")).is_err());
    assert!(!root.join("new").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("e.txt")).unwrap(),
        "read-only-canary\n"
    );
    let extra_write = match ws.sandbox_policy() {
        crate::tools::sandbox::Policy::Strict { extra_write, .. }
        | crate::tools::sandbox::Policy::Dev { extra_write, .. } => extra_write,
    };
    assert!(!extra_write.contains(&root));
    assert!(
        !run(
            &ws,
            "write",
            json!({"path": "local.txt", "content": "local"})
        )
        .is_error
    );
}

#[test]
fn read_only_roots_preserve_private_storage_and_validate_directories() {
    let (_dir, ws) = workspace();
    let extra = tempfile::tempdir().unwrap();
    let root = extra.path().canonicalize().unwrap();
    let private = root.join("private");
    std::fs::create_dir(&private).unwrap();
    std::fs::write(private.join("secret.txt"), "private-canary").unwrap();
    std::fs::write(root.join("public.txt"), "public-canary").unwrap();
    let ws = ws.with_protected_paths(std::slice::from_ref(&private));
    for dir in [
        private.clone(),
        root.join("missing"),
        root.join("public.txt"),
    ] {
        assert!(ws.clone().with_read_only_roots(&[dir]).is_err());
    }
    let filesystem_root = root.ancestors().last().unwrap().to_path_buf();
    assert!(ws.clone().with_read_only_roots(&[filesystem_root]).is_err());
    let ws = ws
        .with_read_only_roots(std::slice::from_ref(&root))
        .unwrap();
    assert!(run(&ws, "read", json!({"path": private.join("secret.txt")})).is_error);
    for (tool, args) in [
        (
            "glob",
            json!({"path": root, "pattern": "**/*", "gitignore": false}),
        ),
        (
            "grep",
            json!({"path": root, "pattern": "canary", "gitignore": false}),
        ),
    ] {
        let out = run(&ws, tool, args);
        assert!(
            !out.is_error && out.text.contains("public.txt"),
            "{tool}: {}",
            out.text
        );
        assert!(
            !out.text.contains("secret.txt") && !out.text.contains("private-canary"),
            "{tool}: {}",
            out.text
        );
    }
}

#[cfg(unix)]
#[test]
fn read_only_roots_do_not_allow_symlink_escapes() {
    let (_dir, ws) = workspace();
    let extra = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "outside-canary").unwrap();
    std::os::unix::fs::symlink(outside.path(), extra.path().join("escape")).unwrap();
    let ws = ws
        .with_read_only_roots(&[extra.path().to_path_buf()])
        .unwrap();
    let path = extra.path().join("escape/secret.txt");
    assert!(run(&ws, "read", json!({"path": path})).is_error);
    let out = run(
        &ws,
        "grep",
        json!({"path": extra.path(), "pattern": "canary"}),
    );
    assert!(
        !out.is_error && !out.text.contains("outside-canary"),
        "{}",
        out.text
    );
}
