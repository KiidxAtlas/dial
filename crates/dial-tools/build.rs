//! Embeds the release-bundled Node.js runtime when `DIAL_BUNDLED_NODE_DIR` names a prepared
//! directory. The directory holds `node.gz` (one gzip-compressed executable) and `metadata.txt`
//! with four lines: Rust target triple, Node version (`x.y.z`), lowercase SHA-256 of the
//! uncompressed executable, and its uncompressed byte count.
//!
//! Only std is used here. Digest and length of the decompressed program are checked by the
//! preparer and again at runtime before anything executes, so this script validates shape and
//! target only. Without the variable the generated constants are absent and source builds keep
//! the PATH fallback.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const ENV: &str = "DIAL_BUNDLED_NODE_DIR";
const MAX_BYTES: u64 = 256 * 1024 * 1024;

fn main() {
    println!("cargo:rerun-if-env-changed={ENV}");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let generated = match env::var_os(ENV).filter(|value| !value.is_empty()) {
        None => absent(),
        Some(dir) => match payload(Path::new(&dir)) {
            Ok(text) => text,
            Err(error) => panic!("{ENV}: {error}"),
        },
    };
    fs::write(out_dir.join("bundled_node.rs"), generated).expect("write bundled_node.rs");
}

fn absent() -> String {
    "pub const VERSION: Option<&str> = None;\n\
     pub const SHA256: &str = \"\";\n\
     pub const SIZE: u64 = 0;\n\
     pub const PAYLOAD: &[u8] = &[];\n"
        .to_owned()
}

fn payload(dir: &Path) -> Result<String, String> {
    let target = env::var("TARGET").map_err(|_| "cargo did not set TARGET".to_owned())?;
    let dir = fs::canonicalize(dir).map_err(|e| format!("cannot open {}: {e}", dir.display()))?;
    let metadata_path = dir.join("metadata.txt");
    let gz_path = dir.join("node.gz");
    println!("cargo:rerun-if-changed={}", metadata_path.display());
    println!("cargo:rerun-if-changed={}", gz_path.display());

    let metadata = fs::read_to_string(&metadata_path)
        .map_err(|e| format!("cannot read {}: {e}", metadata_path.display()))?;
    let lines: Vec<&str> = metadata.lines().collect();
    let [triple, version, sha256, size] = lines.as_slice() else {
        return Err(format!(
            "metadata.txt needs exactly 4 lines (target, version, sha256, size), found {}",
            lines.len()
        ));
    };
    if *triple != target {
        return Err(format!(
            "payload is for target {triple:?} but this build targets {target:?}"
        ));
    }
    if !is_numeric_version(version) {
        return Err(format!("version {version:?} is not x.y.z"));
    }
    if sha256.len() != 64
        || !sha256
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err("sha256 must be 64 lowercase hex digits".to_owned());
    }
    let size: u64 = size
        .parse()
        .map_err(|_| format!("size {size:?} is not a byte count"))?;
    if size == 0 || size > MAX_BYTES {
        return Err(format!(
            "size {size} must be between 1 and {MAX_BYTES} bytes"
        ));
    }

    let gz = fs::read(&gz_path).map_err(|e| format!("cannot read {}: {e}", gz_path.display()))?;
    if gz.len() < 2 || gz[..2] != [0x1f, 0x8b] {
        return Err("node.gz is not gzip data".to_owned());
    }
    if gz.len() as u64 > MAX_BYTES {
        return Err(format!("node.gz exceeds {MAX_BYTES} bytes"));
    }

    Ok(format!(
        "pub const VERSION: Option<&str> = Some({version:?});\n\
         pub const SHA256: &str = {sha256:?};\n\
         pub const SIZE: u64 = {size};\n\
         pub const PAYLOAD: &[u8] = include_bytes!({:?});\n",
        gz_path.display().to_string()
    ))
}

fn is_numeric_version(text: &str) -> bool {
    let parts: Vec<&str> = text.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}
