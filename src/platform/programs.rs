//! Resolve operator-installed programs without assuming POSIX executable bits.
use std::path::{Path, PathBuf};

pub fn executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(windows)]
    {
        extensions().iter().any(|ext| {
            path.extension().is_some_and(|actual| {
                actual
                    .to_string_lossy()
                    .eq_ignore_ascii_case(ext.trim_start_matches('.'))
            })
        })
    }
}
#[cfg(windows)]
fn extensions() -> Vec<String> {
    std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
        .split(';')
        .filter(|ext| !ext.is_empty())
        .map(str::to_owned)
        .collect()
}
pub fn find(program: &Path, directories: &[PathBuf]) -> Option<PathBuf> {
    let candidates: Vec<_> = if program.components().count() > 1 {
        vec![program.to_owned()]
    } else {
        directories
            .iter()
            .map(|directory| directory.join(program))
            .collect()
    };
    for candidate in candidates {
        if executable(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        if candidate.extension().is_none() {
            for extension in extensions() {
                let mut name = candidate.as_os_str().to_os_string();
                name.push(extension);
                let path = PathBuf::from(name);
                if executable(&path) {
                    return Some(path);
                }
            }
        }
    }
    None
}
pub fn bash() -> Result<PathBuf, String> {
    #[cfg(unix)]
    {
        Ok(PathBuf::from("/bin/bash"))
    }
    #[cfg(windows)]
    {
        use winreg::{RegKey, enums::*};
        let mut candidates = Vec::new();
        if let Some(path) = std::env::var_os("DIAL_BASH") {
            candidates.push(PathBuf::from(path));
        }
        for hive in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
            for view in [KEY_WOW64_64KEY, KEY_WOW64_32KEY] {
                if let Ok(key) = RegKey::predef(hive)
                    .open_subkey_with_flags("SOFTWARE\\GitForWindows", KEY_READ | view)
                {
                    if let Ok(root) = key.get_value::<String, _>("InstallPath") {
                        candidates.push(PathBuf::from(root).join("bin/bash.exe"));
                    }
                }
            }
        }
        for variable in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Some(root) = std::env::var_os(variable) {
                let root = PathBuf::from(root);
                candidates.push(root.join("Git/bin/bash.exe"));
                candidates.push(root.join("Programs/Git/bin/bash.exe"));
            }
        }
        for directory in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
            if directory.join("git.exe").is_file() {
                candidates.push(directory.join("../bin/bash.exe"));
                candidates.push(directory.join("bash.exe"));
            }
        }
        candidates.into_iter().find_map(|path| {
            let path = path.canonicalize().ok()?;
            // System32 bash.exe is a WSL launcher, not native Git Bash.
            (path.is_file() && !path.to_string_lossy().to_ascii_lowercase().contains("\\system32\\")).then_some(path)
        }).ok_or_else(|| "Native Windows command tools need Git Bash. Install Git for Windows (winget install --id Git.Git -e), restart the terminal, or set DIAL_BASH to its bash.exe.".into())
    }
}
