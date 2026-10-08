//! Native operating-system boundaries shared by the harness.
pub mod fs;
pub mod pipe;
pub mod process;

pub mod programs;

pub fn home_var_os() -> Option<std::ffi::OsString> {
    #[cfg(windows)]
    if let Some(home) = std::env::var_os("USERPROFILE").filter(|home| !home.is_empty()) {
        return Some(home);
    }
    std::env::var_os("HOME")
}
pub fn home_var() -> Result<String, std::env::VarError> {
    home_var_os()
        .ok_or(std::env::VarError::NotPresent)?
        .into_string()
        .map_err(std::env::VarError::NotUnicode)
}
pub fn shell_path(path: &std::path::Path) -> String {
    let path = path.to_string_lossy().into_owned();
    #[cfg(windows)]
    {
        if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
            return format!("//{}", unc.replace('\\', "/"));
        }
        path.strip_prefix(r"\\?\")
            .unwrap_or(&path)
            .replace('\\', "/")
    }
    #[cfg(unix)]
    {
        path
    }
}
