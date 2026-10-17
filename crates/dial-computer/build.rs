use std::{env, path::PathBuf, process::Command};

fn main() {
    emit_desktop_cfgs();
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
