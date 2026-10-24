use std::io;
use std::path::Path;

/// Atomically moves a file or directory, refusing any existing destination (including symlinks).
/// Both names must be on the same filesystem. There is deliberately no copy/delete or
/// check-then-rename fallback: unsupported kernels/filesystems fail without moving either name.
/// This is namespace atomicity, not crash durability or protection from ancestor replacement.
pub fn rename_noreplace(source: &Path, target: &Path) -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let source = CString::new(source.as_os_str().as_bytes())?;
        let target = CString::new(target.as_os_str().as_bytes())?;
        #[cfg(target_os = "linux")]
        let result = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                target.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        #[cfg(target_os = "macos")]
        let result =
            unsafe { libc::renamex_np(source.as_ptr(), target.as_ptr(), libc::RENAME_EXCL) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide = |path: &Path| -> io::Result<Vec<u16>> {
            let mut value: Vec<u16> = path.as_os_str().encode_wide().collect();
            if value.contains(&0) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "path contains NUL",
                ));
            }
            value.push(0);
            Ok(value)
        };
        let source = wide(source)?;
        let target = wide(target)?;
        // No REPLACE_EXISTING, and no COPY_ALLOWED: directories and files have the same contract.
        if unsafe {
            windows_sys::Win32::Storage::FileSystem::MoveFileExW(
                source.as_ptr(),
                target.as_ptr(),
                0,
            )
        } != 0
        {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = (source, target);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "atomic no-replace rename is unavailable",
        ))
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos", windows)))]
mod tests;
