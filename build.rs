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
}
