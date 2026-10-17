use std::{env, path::PathBuf, process::Command};

/// Tracked inputs compiled into the executable: `src` and the other workspace crates, the manifests
/// and build script, and the files outside `src` embedded with `include_str!`/`include_bytes!`.
/// Keep in sync when embedding a new file from outside `src`; the revision half lives in
/// `crates/dial-build-info/build.rs`.
const INPUTS: &[&str] = &[
    "src",
    "crates",
    "Cargo.toml",
    "Cargo.lock",
    "build.rs",
    "assets",
    "runtimes",
    "themes",
    "CHANGELOG.md",
    "examples/benchmarks/efficiency.toml",
    "python/dial_laya_server.py",
    "python/dial_server.py",
    "python/dial_steering.py",
    "python/dial_tool_grammar.py",
    "python/dial_worker.py",
    "python/laya-requirements.txt",
    "python/pyproject.toml",
];

fn main() {
    for path in INPUTS {
        println!("cargo:rerun-if-changed={path}");
    }
    emit_desktop_cfgs();
    build_apple_fm_helper();
}

/// Apple's on-device model helper (`src/llm/apple.rs`) needs the FoundationModels framework, which
/// ships in the macOS 26 SDK. Older SDKs or a missing `swiftc` leave `dial_apple_fm` unset, so the
/// `apple` provider reports itself unsupported instead of failing the build.
fn build_apple_fm_helper() {
    println!("cargo:rustc-check-cfg=cfg(dial_apple_fm)");
    println!("cargo:rerun-if-env-changed=DIAL_NO_APPLE_FM");
    println!("cargo:rerun-if-env-changed=SWIFTC");
    println!("cargo:rerun-if-env-changed=SDKROOT");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos")
        || env::var_os("DIAL_NO_APPLE_FM").is_some()
    {
        return;
    }
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR should be set"));
    let source = manifest_dir.join("runtimes/apple/DialAppleFM.swift");
    let output =
        PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR should be set")).join("dial-apple-fm");
    let architecture = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        Ok("x86_64") => "x86_64",
        _ => return,
    };
    println!("cargo:rerun-if-changed={}", source.display());
    let sdk_root = env::var_os("SDKROOT").map(PathBuf::from).or_else(|| {
        Command::new("xcrun")
            .args(["--sdk", "macosx", "--show-sdk-path"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()))
    });
    let Some(sdk_root) = sdk_root else {
        println!("cargo:warning=Apple on-device model disabled: no macOS SDK found");
        return;
    };
    if !sdk_root
        .join("System/Library/Frameworks/FoundationModels.framework")
        .exists()
    {
        // Older SDKs simply lack the framework; that is a supported configuration, not a warning.
        return;
    }
    let compiler = env::var_os("SWIFTC").unwrap_or_else(|| "swiftc".into());
    let result = Command::new(&compiler)
        .current_dir(&manifest_dir)
        .arg("-sdk")
        .arg(&sdk_root)
        .args([
            "-parse-as-library",
            "-O",
            "-target",
            &format!("{architecture}-apple-macosx26.0"),
            "-Xlinker",
            "-dead_strip",
            "-Xlinker",
            "-adhoc_codesign",
        ])
        .arg(&source)
        .arg("-o")
        .arg(&output)
        .output();
    match result {
        Ok(result) if result.status.success() => {
            println!("cargo:rustc-cfg=dial_apple_fm");
            println!("cargo:rustc-env=DIAL_APPLE_FM_HELPER={}", output.display());
        }
        Ok(result) => println!(
            "cargo:warning=Apple on-device model disabled: swiftc failed ({}): {}",
            result.status,
            String::from_utf8_lossy(&result.stderr)
                .lines()
                .find(|line| line.contains("error"))
                .unwrap_or("see swiftc output")
        ),
        Err(error) => println!(
            "cargo:warning=Apple on-device model disabled: cannot run {compiler:?}: {error}"
        ),
    }
}

/// Native desktop backends (`desktop` feature, default on) are selected by these cfgs in
/// `src/computer/desktop`. Without the feature no cfg is set, so the platform crates and the macOS
/// capture helper are not compiled and every desktop operation reports `Unsupported`.
fn emit_desktop_cfgs() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    for os in ["macos", "windows", "linux"] {
        println!("cargo:rustc-check-cfg=cfg(dial_desktop_{os})");
    }
    if env::var_os("CARGO_FEATURE_DESKTOP").is_none() {
        return;
    }
    if matches!(target_os.as_str(), "macos" | "windows" | "linux") {
        println!("cargo:rustc-cfg=dial_desktop_{target_os}");
    }
    if target_os == "macos" {
        build_darwin_capture_helper();
    }
}

fn build_darwin_capture_helper() {
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR should be set"));
    let source = manifest_dir.join("src/computer/desktop/macos/capture/helper.m");
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR should be set"))
        .join("dial-capture-helper");
    let architecture = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        Ok("x86_64") => "x86_64",
        Ok(architecture) => panic!("unsupported macOS helper architecture: {architecture}"),
        Err(error) => panic!("CARGO_CFG_TARGET_ARCH should be set: {error}"),
    };
    println!("cargo:rerun-if-changed={}", source.display());
    println!("cargo:rerun-if-env-changed=CC");
    println!("cargo:rerun-if-env-changed=SDKROOT");

    let compiler = env::var_os("CC").unwrap_or_else(|| "clang".into());
    let mut command = Command::new(&compiler);
    command.current_dir(&manifest_dir);
    if let Some(sdk_root) = env::var_os("SDKROOT") {
        command.arg("-isysroot").arg(sdk_root);
    }
    command
        .args([
            "-x",
            "objective-c",
            "-fobjc-arc",
            "-fblocks",
            "-fno-ident",
            "-Os",
            "-arch",
            architecture,
            "-Wl,-dead_strip",
            "-Wl,-adhoc_codesign",
            "-mmacosx-version-min=14.0",
        ])
        .args(["-framework", "AppKit"])
        .args(["-framework", "ScreenCaptureKit"])
        .args(["-framework", "CoreGraphics"]);
    let result = command
        .arg(&source)
        .arg("-o")
        .arg(&output)
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "failed to invoke clang ({compiler:?}) for dial-capture-helper; install Xcode \
                 command line tools or set CC: {error}"
            )
        });
    assert!(
        result.status.success(),
        "failed to build macOS dial-capture-helper ({}):\nstdout:\n{}\nstderr:\n{}",
        result.status,
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr),
    );
    println!(
        "cargo:rustc-env=DIAL_CAPTURE_DARWIN_HELPER={}",
        output.display()
    );
}
