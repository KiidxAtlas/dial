use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

/// Tracked inputs compiled into the executable: `src`, the manifests and build script, and the
/// files outside `src` embedded with `include_str!`/`include_bytes!` (keep in sync when embedding
/// a new file from outside `src`). The script reruns only when these or the checked-out commit
/// change, and `DIRTY` reports whether these inputs differ from that commit.
///
/// Do not watch `.git/index`: Git, editors and dial itself rewrite it on every status refresh, and
/// each rerun of this script recompiles the whole crate even when no source changed.
const INPUTS: &[&str] = &[
    "src",
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

fn git(args: &[&str]) -> Option<std::process::Output> {
    Command::new("git")
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .current_dir(env::var_os("CARGO_MANIFEST_DIR")?)
        .output()
        .ok()
}

/// Watches an existing path; Cargo treats a missing path as always changed, which would rerun
/// this script and recompile the crate on every build.
fn rerun_if_exists(path: &Path) {
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn main() {
    for name in ["DIAL_BUILD_REVISION", "SOURCE_DATE_EPOCH"] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    for path in INPUTS {
        println!("cargo:rerun-if-changed={path}");
    }
    // A linked worktree keeps HEAD in its own Git directory and branch refs in the common one.
    if let Some(output) = git(&["rev-parse", "--git-dir", "--git-common-dir"])
        .filter(|output| output.status.success())
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut dirs = stdout.lines().map(|line| PathBuf::from(line.trim()));
        if let (Some(git_dir), Some(common_dir)) = (dirs.next(), dirs.next()) {
            rerun_if_exists(&git_dir.join("HEAD"));
            rerun_if_exists(&common_dir.join("refs"));
            rerun_if_exists(&common_dir.join("packed-refs"));
        }
    }
    let revision = env::var("DIAL_BUILD_REVISION")
        .ok()
        .or_else(|| {
            git(&["rev-parse", "--short=12", "HEAD"])
                .filter(|output| output.status.success())
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".into());
    assert!(
        revision == "unknown"
            || ((7..=40).contains(&revision.len())
                && revision.bytes().all(|byte| byte.is_ascii_hexdigit())),
        "DIAL_BUILD_REVISION must be a Git hash or unknown"
    );
    // A shipped binary without its commit makes bug reports and version checks useless. Dev and
    // local-profile builds keep the fallback for vendored sources without a Git checkout.
    if env::var("PROFILE").as_deref() == Ok("release") {
        assert!(
            revision != "unknown",
            "release builds need a revision: build from a Git checkout or set DIAL_BUILD_REVISION"
        );
    }
    let diff_args = [
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--quiet",
        "HEAD",
        "--",
    ];
    let dirty =
        match git(&[&diff_args[..], INPUTS].concat()).and_then(|output| output.status.code()) {
            Some(0) => "false",
            Some(1) => "true",
            _ => "unknown",
        };
    let epoch = env::var("SOURCE_DATE_EPOCH")
        .map(|value| {
            value
                .parse::<u64>()
                .expect("SOURCE_DATE_EPOCH must be an integer")
        })
        .unwrap_or_else(|_| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock before Unix epoch")
                .as_secs()
        });
    for (name, value) in [
        ("REVISION", revision),
        ("DIRTY", dirty.into()),
        ("EPOCH", epoch.to_string()),
        ("TARGET", env::var("TARGET").unwrap()),
        ("PROFILE", env::var("PROFILE").unwrap()),
    ] {
        println!("cargo:rustc-env=DIAL_BUILD_{name}={value}");
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
