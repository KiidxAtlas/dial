use super::*;

#[test]
fn manifest_is_pinned_to_exact_versions() {
    assert!(PACKAGE.contains("\"playwright-core\": \"1.64.0\""));
    assert!(PACKAGE.contains("\"@mozilla/readability\": \"0.6.0\""));
    assert!(LOCK.contains("playwright-core@1.64.0"));
    assert!(LOCK.contains("@mozilla/readability@0.6.0"));
}

#[test]
fn digest_dir_is_stable_and_hex() {
    let first = digest_dir();
    assert_eq!(first, digest_dir());
    assert_eq!(first.len(), 16);
    assert!(first.bytes().all(|b| b.is_ascii_hexdigit()));
}

#[test]
fn layout_points_at_playwright_and_readability_inside_node_modules() {
    let runtime = layout(Path::new("/cache/r"));
    assert_eq!(
        runtime.entry,
        Path::new("/cache/r/node_modules/playwright-core/index.js")
    );
    assert_eq!(
        runtime.readability,
        Path::new("/cache/r/node_modules/@mozilla/readability/Readability.js")
    );
    assert_eq!(runtime.browsers, Path::new("/cache/r/browsers"));
}

#[test]
fn provision_cached_dependencies_offline_without_private_chromium() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = layout(dir.path());
    for path in [&runtime.entry, &runtime.readability] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "// cached dependency").unwrap();
    }
    assert!(!runtime.browsers.exists());
    // Configured Chromium and CDP startup both provision these same dependencies only.
    let provisioned = provision_at(dir.path(), &AtomicBool::new(false), || {
        panic!("cached dependencies must not probe the network")
    })
    .unwrap();
    assert_eq!(provisioned.entry, runtime.entry);
    assert!(!runtime.browsers.exists());
    std::fs::remove_file(&runtime.readability).unwrap();
    let error = provision_at(dir.path(), &AtomicBool::new(false), || false).unwrap_err();
    assert!(
        error.contains("JavaScript dependencies are not cached"),
        "{error}"
    );
    assert!(error.contains("registry.npmjs.org"), "{error}");
}

#[test]
fn cached_runtime_skips_the_network_probe() {
    let probed = std::cell::Cell::new(false);
    let result = ensure_fetchable(true, || {
        probed.set(true);
        false
    });
    assert!(result.is_ok());
    assert!(!probed.get(), "a cached runtime must not probe the network");
}

#[test]
fn uncached_runtime_fetches_when_hosts_are_reachable() {
    assert!(ensure_fetchable(false, || true).is_ok());
}

#[test]
fn uncached_runtime_fails_clearly_when_offline() {
    let error = ensure_fetchable(false, || false).unwrap_err();
    assert!(error.contains("not cached"), "{error}");
    assert!(error.contains("registry.npmjs.org"), "{error}");
    assert!(error.contains("connect to the network"), "{error}");
}

#[test]
fn cache_only_prerequisites_never_create_missing_dependencies() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        cached_at(dir.path())
            .unwrap_err()
            .contains("installation is disabled")
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    let runtime = layout(dir.path());
    for path in [&runtime.entry, &runtime.readability] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "// existing runtime").unwrap();
    }
    assert_eq!(cached_at(dir.path()).unwrap().entry, runtime.entry);
    assert!(!dir.path().join("package.json").exists());
    assert!(!runtime.browsers.exists());
}
