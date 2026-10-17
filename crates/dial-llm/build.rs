use std::{env, path::PathBuf, process::Command};

fn main() {
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
    let source = manifest_dir.join("../../runtimes/apple/DialAppleFM.swift");
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
