//! Offline real-filesystem benchmark (no dependencies beyond dial-core).
//! Run: cargo run -p dial-core --release --example workspace_copy_bench -- 9
//! Fixtures are deterministic; copy timing excludes setup, validation and TempDir cleanup.
//! Results are warm-cache wall times, not disk throughput or cold-cache measurements.
use anyhow::{Result, ensure};
use dial_core::workspace_copy::{clone_project, copy_project_plain};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

type Manifest = BTreeMap<PathBuf, Vec<u8>>;
type Copier = fn(&Path) -> Result<tempfile::TempDir>;

fn write(root: &Path, name: &str, content: &[u8], expected: &mut Manifest) -> Result<()> {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(path, content)?;
    expected.insert(PathBuf::from(name), content.to_vec());
    Ok(())
}

fn fixture(kind: &str) -> Result<(tempfile::TempDir, Manifest)> {
    let source = tempfile::tempdir()?;
    let root = source.path();
    let mut expected = Manifest::new();
    match kind {
        "small" => {
            for i in 0..1200 {
                write(
                    root,
                    &format!("file-{i:04}.rs"),
                    format!("// source {i}\n").as_bytes(),
                    &mut expected,
                )?;
            }
        }
        "nested" => {
            for package in 0..30 {
                for i in 0..20 {
                    write(
                        root,
                        &format!("packages/p{package}/src/module/deep/leaf/f{i}.rs"),
                        &[b'x'; 1024],
                        &mut expected,
                    )?;
                }
            }
        }
        "ignored" => {
            for i in 0..100 {
                write(root, &format!("src/f{i}.rs"), &[b'x'; 512], &mut expected)?;
            }
            for excluded in ["node_modules", "target", ".git", ".dial"] {
                for i in 0..500 {
                    let path = root.join(format!("{excluded}/package-{i}/data"));
                    fs::create_dir_all(path.parent().unwrap())?;
                    fs::write(path, [b'z'; 1024])?;
                }
            }
            fs::write(root.join(".env"), "secret")?;
            write(root, ".env.example", b"template", &mut expected)?;
        }
        "symlinks" => {
            for i in 0..100 {
                write(root, &format!("real/f{i}.rs"), &[b'y'; 256], &mut expected)?;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::symlink;
                for alias in 0..4 {
                    symlink("real", root.join(format!("alias-{alias}")))?;
                    for i in 0..100 {
                        expected.insert(
                            PathBuf::from(format!("alias-{alias}/f{i}.rs")),
                            vec![b'y'; 256],
                        );
                    }
                }
                symlink("real/f0.rs", root.join("file-alias"))?;
                expected.insert(PathBuf::from("file-alias"), vec![b'y'; 256]);
                symlink(".", root.join("cycle"))?;
                fs::write(root.join(".env"), "secret")?;
                symlink(".env", root.join("secret-alias"))?;
            }
        }
        _ => unreachable!(),
    }
    Ok((source, expected))
}

fn manifest(root: &Path, dir: &Path, output: &mut Manifest) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        ensure!(!kind.is_symlink(), "copy must materialize symlinks");
        if kind.is_dir() {
            manifest(root, &path, output)?;
        } else {
            ensure!(kind.is_file(), "unexpected special file");
            output.insert(path.strip_prefix(root)?.to_owned(), fs::read(path)?);
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let rounds: usize = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "9".into())
        .parse()?;
    ensure!(rounds >= 3, "at least three measured rounds are required");
    println!("case,backend,rounds,files,bytes,min_ms,median_ms,max_ms");
    for case in ["small", "nested", "ignored", "symlinks"] {
        let (source, expected) = fixture(case)?;
        let backends: [(&str, Copier); 2] =
            [("clone", clone_project), ("plain", copy_project_plain)];
        let mut samples = [Vec::new(), Vec::new()];
        // Warm both paths once, then alternate first backend to reduce ordering bias.
        for round in 0..=rounds {
            for offset in 0..2 {
                let index = (round + offset) % 2;
                let start = Instant::now();
                let copied = backends[index].1(source.path())?;
                let elapsed = start.elapsed().as_secs_f64() * 1000.0;
                let mut actual = Manifest::new();
                manifest(copied.path(), copied.path(), &mut actual)?;
                ensure!(
                    actual == expected,
                    "{case}/{} output mismatch",
                    backends[index].0
                );
                #[cfg(unix)]
                if case == "symlinks" {
                    ensure!(
                        fs::read_dir(copied.path().join("cycle"))?.count() == 0,
                        "cycle must stop"
                    );
                }
                if round > 0 {
                    samples[index].push(elapsed);
                }
            }
        }
        for (index, (backend, _)) in backends.iter().enumerate() {
            samples[index].sort_by(f64::total_cmp);
            let times = &samples[index];
            println!(
                "{case},{backend},{rounds},{},{},{:.3},{:.3},{:.3}",
                expected.len(),
                expected.values().map(Vec::len).sum::<usize>(),
                times[0],
                times[rounds / 2],
                times[rounds - 1]
            );
        }
    }
    Ok(())
}
