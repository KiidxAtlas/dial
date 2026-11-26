use super::*;

#[test]
fn parses_bun_versions() {
    assert_eq!(parse_version("1.3.14"), Some((1, 3, 14)));
    assert_eq!(parse_version("bun 1.2.9\n"), Some((1, 2, 9)));
    assert_eq!(parse_version("nothing here"), None);
    assert!(parse_version("1.2.9").unwrap() < BUN_MIN);
}

#[test]
fn cancelling_after_a_step_starts_terminates_its_descendant() {
    #[cfg(unix)]
    let (program, args) = ("sh", vec!["-c", "sleep 15 & echo $!; wait"]);
    #[cfg(windows)]
    let (program, args) = (
        "powershell.exe",
        vec![
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$p = Start-Process powershell.exe -ArgumentList '-NoProfile -NonInteractive -Command Start-Sleep -Seconds 15' -PassThru; $p.Id; $p.WaitForExit()",
        ],
    );
    let step = Step {
        program: program.into(),
        args: args.into_iter().map(String::from).collect(),
        env: Vec::new(),
    };
    let cancel = AtomicBool::new(false);
    let mut descendant = None;
    let error = run_step(&step, &cancel, &mut |line| {
        if let Ok(pid) = line.parse::<i32>() {
            descendant = crate::platform::procinfo::ProcId::of(pid);
            cancel.store(true, Ordering::Release);
        }
    })
    .unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error}");
    let child = descendant.expect("observed running descendant");
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while child.alive() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!child.alive(), "step's descendant survived cancellation");
}

#[cfg(unix)]
#[test]
fn python_voice_health_uses_supported_help_and_propagates_failure() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let program = tmp.path().join("voice-cli");
    std::fs::write(
        &program,
        "#!/bin/sh\n[ \"$#\" = 1 ] && [ \"$1\" = --help ]\n",
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    for id in ["piper", "mlx-whisper"] {
        health(id, &program, None).unwrap();
    }
    std::fs::write(&program, "#!/bin/sh\nexit 7\n").unwrap();
    for id in ["piper", "mlx-whisper"] {
        assert!(
            health(id, &program, None).is_err(),
            "{id} must not hide a broken CLI"
        );
    }
}

#[test]
fn summary_counts_missing_capabilities_on_one_line_and_the_full_list_names_each() {
    let tmp = tempfile::tempdir().unwrap();
    let env = Env {
        path: Vec::new(),
        user_dir: tmp.path().join("dial"),
    };
    let full = list_text(&env, "dial doctor --fix");
    let missing = full.lines().filter(|line| line.starts_with("  · ")).count();
    assert!(missing > 0, "{full}");
    assert!(full.contains("dial doctor --fix bun"), "{full}");
    let summary = summary_text(&env, "dial doctor --all");
    assert!(
        summary.contains(&format!("Not installed: {missing} optional capabilit")),
        "{summary}"
    );
    assert!(
        summary.contains("`dial doctor --all` lists them"),
        "{summary}"
    );
    assert!(
        !summary.lines().any(|line| line.starts_with("  · ")),
        "{summary}"
    );
}

#[test]
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[ignore = "downloads the pinned MLX-Serve release (about 100 MB) into a temporary user folder"]
fn installs_the_pinned_mlx_serve_and_it_answers() {
    let user_dir = tempfile::tempdir().unwrap();
    let env = Env {
        path: Vec::new(),
        user_dir: user_dir.path().to_path_buf(),
    };
    let location = install("mlx-serve", &env, &AtomicBool::new(false), &mut |_| {}).unwrap();
    assert!(
        location.starts_with(user_dir.path()),
        "{}",
        location.display()
    );
    let version = probe(&location, &["--version"], &AtomicBool::new(false)).unwrap();
    assert!(version.contains("mlx-serve 26.11.3"), "{version}");
}
