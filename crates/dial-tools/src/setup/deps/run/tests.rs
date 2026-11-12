use super::*;

#[test]
fn parses_bun_versions() {
    assert_eq!(parse_version("1.3.14"), Some((1, 3, 14)));
    assert_eq!(parse_version("bun 1.2.9\n"), Some((1, 2, 9)));
    assert_eq!(parse_version("nothing here"), None);
    assert!(parse_version("1.2.9").unwrap() < BUN_MIN);
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
