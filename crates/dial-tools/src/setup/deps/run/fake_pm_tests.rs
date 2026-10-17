use super::*;
use crate::setup::deps::{install_dir, plan::tests::fake_tools};
use std::os::unix::fs::PermissionsExt;

/// A stand-in `npm`: creates `<prefix>/node_modules/.bin/bun`, whose `--version` prints
/// `version`. It echoes its argv so the test can check the exact command.
fn fake_npm(root: &Path, version: &str) {
    let bin = root.join("fakebin/npm");
    let script = format!(
        "#!/bin/sh\nset -eu\nprefix=\"\"\nwhile [ $# -gt 0 ]; do case \"$1\" in --prefix) prefix=\"$2\"; shift 2;; install|--no-audit|--no-fund) shift;; *) break;; esac; done\n/bin/mkdir -p \"$prefix/node_modules/.bin\"\nprintf '#!/bin/sh\\necho {version}\\n' > \"$prefix/node_modules/.bin/bun\"\n/bin/chmod +x \"$prefix/node_modules/.bin/bun\"\necho \"added 1 package: $*\"\n"
    );
    std::fs::write(&bin, script).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn install_runs_fake_npm_into_dial_dir_and_health_checks_bun() {
    let tmp = tempfile::tempdir().unwrap();
    let env = fake_tools(tmp.path(), &["npm"]);
    fake_npm(tmp.path(), "1.3.14");
    let mut lines = Vec::new();
    let location = install("bun", &env, &AtomicBool::new(false), &mut |line| {
        lines.push(line.to_owned())
    })
    .unwrap();
    assert_eq!(
        location,
        install_dir(&env.user_dir, "bun").join("node_modules/.bin/bun")
    );
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("$ ") && line.contains("--prefix"))
    );
    assert!(
        lines.iter().any(|line| line == "ok: Bun 1.3.14"),
        "{lines:?}"
    );
}

#[test]
fn install_rejects_bun_older_than_the_minimum() {
    let tmp = tempfile::tempdir().unwrap();
    let env = fake_tools(tmp.path(), &["npm"]);
    fake_npm(tmp.path(), "1.2.9");
    let error = install("bun", &env, &AtomicBool::new(false), &mut |_| {}).unwrap_err();
    assert!(
        error.to_string().contains("older than the required 1.3.14"),
        "{error}"
    );
}

#[test]
fn cancel_stops_a_running_step() {
    let tmp = tempfile::tempdir().unwrap();
    let slow = tmp.path().join("slow");
    std::fs::write(&slow, "#!/bin/sh\nsleep 30\n").unwrap();
    std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
    let step = Step {
        program: slow.display().to_string(),
        args: Vec::new(),
        env: Vec::new(),
    };
    let error = run_step(&step, &AtomicBool::new(true), &mut |_| {}).unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error}");
}
