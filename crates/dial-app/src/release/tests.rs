use super::*;

#[test]
fn installation_detection_preserves_package_manager_ownership() {
    assert_eq!(
        Installation::of(Path::new("/opt/homebrew/Cellar/dial/bin/dial")),
        Installation::Homebrew
    );
    assert_eq!(
        Installation::of(Path::new("/home/user/.cargo/bin/dial")),
        Installation::Cargo
    );
    assert_eq!(
        Installation::of(Path::new("/nix/store/hash-dial/bin/dial")),
        Installation::Nix
    );
    assert_eq!(
        Installation::of(Path::new("/home/user/.local/bin/dial")),
        Installation::Script
    );
    assert_eq!(
        Installation::of(Path::new(r"C:\Users\user\.cargo\bin\dial.exe")),
        Installation::Cargo
    );
}

#[test]
fn cargo_installations_update_from_the_repository_not_crates_io() {
    let command = Installation::Cargo.update_command();
    assert!(
        command.contains(&format!("--git https://github.com/{REPOSITORY}")),
        "{command}"
    );
    assert_eq!(Installation::Script.update_command(), "dial update");
    let pinned = Installation::Cargo
        .package_manager_argv(Some("v0.2.8"))
        .unwrap();
    assert_eq!(pinned[pinned.len() - 3..], ["--tag", "v0.2.8", "dial"]);
    assert_eq!(
        Installation::Homebrew.package_manager_argv(None).unwrap(),
        ["brew", "upgrade", "dial"]
    );
    assert!(Installation::Nix.package_manager_argv(None).is_none());
    assert!(Installation::Script.package_manager_argv(None).is_none());
}

#[test]
fn every_installation_has_a_plan_and_source_builds_rebuild_their_checkout() {
    let checkout = Path::new("/src/dial");
    let rebuild = UpdatePlan::Checkout(checkout.to_owned());
    // Installer copies verify signed releases; a key always wins over a leftover checkout.
    assert_eq!(
        plan(Installation::Script, true, Some(checkout)),
        UpdatePlan::Release
    );
    assert_eq!(plan(Installation::Script, false, Some(checkout)), rebuild);
    // `make update` refreshes ~/.cargo/bin/dial too, so that copy is rebuilt, not reinstalled.
    assert_eq!(plan(Installation::Cargo, false, Some(checkout)), rebuild);
    assert_eq!(
        plan(Installation::Cargo, false, None),
        UpdatePlan::PackageManager(Installation::Cargo)
    );
    for verified in [false, true] {
        assert_eq!(
            plan(Installation::Homebrew, verified, Some(checkout)),
            UpdatePlan::PackageManager(Installation::Homebrew)
        );
        assert!(matches!(
            plan(Installation::Nix, verified, Some(checkout)),
            UpdatePlan::Manual(_)
        ));
    }
    let UpdatePlan::Manual(how) = plan(Installation::Script, false, None) else {
        panic!("a keyless build without its checkout cannot update itself");
    };
    assert!(how.contains("releases/latest"), "{how}");
}

#[test]
fn only_a_dial_source_checkout_outside_cargo_counts() {
    let root = tempfile::tempdir().unwrap();
    let checkout = root.path().join("dial");
    std::fs::create_dir_all(checkout.join(".git")).unwrap();
    std::fs::create_dir_all(checkout.join("scripts")).unwrap();
    assert_eq!(checkout_at(&checkout), None, "no Makefile yet");
    std::fs::write(checkout.join("Makefile"), "").unwrap();
    std::fs::write(checkout.join("scripts/install-dev.py"), "").unwrap();
    assert_eq!(checkout_at(&checkout), Some(checkout.clone()));
    std::fs::remove_dir(checkout.join(".git")).unwrap();
    assert_eq!(
        checkout_at(&checkout),
        None,
        "an exported tree is not a checkout"
    );

    let cargo = root.path().join(".cargo/git/checkouts/dial-1234/abcdef");
    std::fs::create_dir_all(cargo.join(".git")).unwrap();
    std::fs::create_dir_all(cargo.join("scripts")).unwrap();
    std::fs::write(cargo.join("Makefile"), "").unwrap();
    std::fs::write(cargo.join("scripts/install-dev.py"), "").unwrap();
    assert_eq!(
        checkout_at(&cargo),
        None,
        "cargo install owns its checkouts"
    );
}

#[test]
fn tags_are_normalized_and_path_safe() {
    assert_eq!(tag("0.2.7").unwrap(), "v0.2.7");
    assert_eq!(tag("v0.2.7-rc.1").unwrap(), "v0.2.7-rc.1");
    assert!(tag("v").is_err());
    assert!(tag("v0.2/../x").is_err());
    assert!(tag("0.2.7\n").is_err());
}

#[test]
fn only_strictly_newer_releases_are_reported() {
    assert!(is_newer("v0.2.7", "0.2.6"));
    assert!(is_newer("v0.10.0", "0.9.9"));
    assert!(is_newer("v1.0.0", "0.99.99"));
    assert!(!is_newer("v0.2.6", "0.2.6"));
    assert!(!is_newer("v0.2.5", "0.2.6"));
    // A final release supersedes its own pre-release, never the other way round.
    assert!(is_newer("v0.3.0", "0.3.0-rc.1"));
    assert!(!is_newer("v0.3.0-rc.1", "0.3.0"));
    assert!(!is_newer("v0.3.0-rc.2", "0.3.0-rc.1"));
    assert!(!is_newer("v0.2.6+build", "0.2.6"));
    assert!(!is_newer("garbage", "0.2.6"));
    assert!(!is_newer("v0.3", "0.2.6"));
    assert!(!is_newer("v0.3.0.1", "0.2.6"));
}
