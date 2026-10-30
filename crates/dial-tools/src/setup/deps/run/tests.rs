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
