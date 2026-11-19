//! Optional, offline embedding of release-prepared Node and CPython payloads. Both use a
//! normalized gzip payload and four-line metadata (target, version, raw SHA-256, raw size).
//! Node verifies its executable on extraction and reuse. CPython's tar stream is verified here;
//! the runtime extracts a fresh private tree from these exact bytes for each interpreter.

use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

const MAX_BYTES: u64 = 256 * 1024 * 1024;

fn main() {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    for (variable, name, filename) in [
        ("DIAL_BUNDLED_NODE_DIR", "node", "node.gz"),
        ("DIAL_BUNDLED_PYTHON_DIR", "python", "python.tar.gz"),
    ] {
        println!("cargo:rerun-if-env-changed={variable}");
        let generated = match env::var_os(variable).filter(|v| !v.is_empty()) {
            None => absent(),
            Some(dir) => payload(Path::new(&dir), filename, &out_dir)
                .unwrap_or_else(|e| panic!("{variable}: {e}")),
        };
        fs::write(out_dir.join(format!("bundled_{name}.rs")), generated)
            .expect("write bundle metadata");
    }
}

fn absent() -> String {
    "pub const VERSION: Option<&str> = None;\n\
     pub const SHA256: &str = \"\";\n\
     pub const SIZE: u64 = 0;\n\
     pub const PAYLOAD: &[u8] = &[];\n"
        .to_owned()
}

fn payload(dir: &Path, filename: &str, out_dir: &Path) -> Result<String, String> {
    let target = env::var("TARGET").map_err(|_| "cargo did not set TARGET".to_owned())?;
    let dir = fs::canonicalize(dir).map_err(|e| format!("cannot open {}: {e}", dir.display()))?;
    let metadata_path = dir.join("metadata.txt");
    let gz_path = dir.join(filename);
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
        return Err(format!("{filename} is not gzip data"));
    }
    if gz.len() as u64 > MAX_BYTES {
        return Err(format!("{filename} exceeds {MAX_BYTES} bytes"));
    }
    let gz_path = if filename == "python.tar.gz" {
        let mut raw = flate2::read::GzDecoder::new(gz.as_slice()).take(size + 1);
        let mut digest = Sha256::new();
        let n = io::copy(&mut raw, &mut digest).map_err(|e| format!("decoding {filename}: {e}"))?;
        if n != size || format!("{:x}", digest.finalize()) != *sha256 {
            return Err(format!("{filename} raw digest or length mismatch"));
        }
        // Include the bytes actually verified, not an input that could change before rustc reads it.
        let dest = out_dir.join(filename);
        fs::write(&dest, &gz).map_err(|e| format!("staging {filename}: {e}"))?;
        dest
    } else {
        gz_path
    };

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
