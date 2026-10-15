use std::{
    env,
    path::PathBuf,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

fn git(args: &[&str]) -> Option<std::process::Output> {
    Command::new("git")
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .current_dir(env::var_os("CARGO_MANIFEST_DIR")?)
        .output()
        .ok()
}

fn main() {
    for name in ["DIAL_BUILD_REVISION", "SOURCE_DATE_EPOCH"] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    for path in ["src", "Cargo.toml", "Cargo.lock", "build.rs"] {
        println!("cargo:rerun-if-changed={path}");
    }
    if let Some(output) = git(&["rev-parse", "--git-dir"]).filter(|output| output.status.success())
    {
        let dir = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        for path in ["HEAD", "index", "refs", "packed-refs"] {
            println!("cargo:rerun-if-changed={}", dir.join(path).display());
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
    let dirty = match git(&["diff", "--no-ext-diff", "--no-textconv", "--quiet", "HEAD"])
        .and_then(|output| output.status.code())
    {
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
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
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
