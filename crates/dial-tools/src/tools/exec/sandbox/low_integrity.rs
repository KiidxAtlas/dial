//! Windows Dev confinement: commands run at Low integrity.
//!
//! Dial re-executes itself with [`LAUNCH_ARG`]. That launcher lowers its own token to the Low
//! mandatory level and then starts the requested program (PowerShell, Git Bash or the built-in
//! shell), which inherits the Low token, the job object and stdio. Windows refuses a Low process
//! any write to an object labeled Medium, which is the implicit label of ordinary files, so:
//!
//! * the workspace and each `extra_write` root get an inheritable Low label and stay writable
//!   (create, edit, delete);
//! * each `deny_read` path that exists (secret directories, Dial's own state) gets an explicit
//!   Medium label with no-read-up, so Low commands can neither read nor write it;
//! * existing Git metadata inside a writable root gets an explicit Medium label, so hooks and
//!   config cannot be changed in place.
//!
//! The labels are stored in the files' security descriptors and stay after Dial exits. They do
//! not affect normal (Medium) programs. Labeling a root propagates to every existing file below
//! it once; later files inherit the label when they are created. A root whose own label already
//! matches is not walked again.
//!
//! [`probe`] runs a fixed PowerShell enforcement test through the real launcher and reports the
//! sandbox unavailable unless every check holds; there is no unsandboxed fallback.
//!
//! With [`CHILD_FENCE_ARG`] (alone, or after [`LAUNCH_ARG`]) the launcher creates the program with
//! `PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY`, so the program cannot start processes of its own.
//! `eval` kernels use it as their process-fork fence, sandboxed or not.
//!
//! Residual limits:
//! * Reads are not confined beyond the `deny_read` paths, as in macOS Dev mode.
//! * Network is not confined, so policies without network (`strict`, `allow_network = false`)
//!   and domain filtering (`network_allow`/`network_deny`) are refused rather than weakened.
//! * A Low process that may delete entries of a writable directory can rename a protected `.git`
//!   directory out of the way and create a new one; only in-place changes are prevented.
//! * Files moved (not copied) into a writable root keep their previous label.
//! * Toolchains that write caches outside the project (`~/.cargo`, npm's AppData cache) need
//!   those directories in `sandbox.extra_write`.
// Compiled on other platforms only for the policy tests below.
#![cfg_attr(not(windows), allow(dead_code))]

use super::Policy;
use std::path::PathBuf;

/// Hidden first argument that turns a Dial process into the Low-integrity launcher.
pub const LAUNCH_ARG: &str = "--dial-low-integrity";

/// Hidden launcher argument: start the program unable to create child processes.
pub const CHILD_FENCE_ARG: &str = "--dial-child-fence";

/// The `CreateProcessW` command line for `program args…`, quoted for the Microsoft C runtime
/// argument parser. The program token cannot contain quotes, so it is quoted verbatim.
fn command_line(program: &str, args: &[String]) -> Result<String, String> {
    if program.contains('"') {
        return Err("program path contains a quote".into());
    }
    let mut line = format!("\"{program}\"");
    for arg in args {
        line.push(' ');
        if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\u{b}', '"']) {
            line.push_str(arg);
            continue;
        }
        line.push('"');
        let mut backslashes = 0;
        for c in arg.chars() {
            match c {
                '\\' => backslashes += 1,
                '"' => {
                    line.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                    line.push('"');
                    backslashes = 0;
                }
                c => {
                    line.extend(std::iter::repeat_n('\\', backslashes));
                    line.push(c);
                    backslashes = 0;
                }
            }
        }
        line.extend(std::iter::repeat_n('\\', backslashes * 2));
        line.push('"');
    }
    Ok(line)
}

/// The mandatory label a path receives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Label {
    /// Low: Low-integrity commands may read and write it.
    Writable,
    /// Medium, no-write-up: readable but not writable by Low commands.
    ReadOnly,
    /// Medium, no-read-up and no-write-up: neither readable nor writable by Low commands.
    Hidden,
}

impl Label {
    /// The SDDL mandatory-label ACE; directories pass it on to their contents.
    fn ace(self, directory: bool) -> &'static str {
        match (self, directory) {
            (Label::Writable, true) => "(ML;OICI;NW;;;LW)",
            (Label::Writable, false) => "(ML;;NW;;;LW)",
            (Label::ReadOnly, true) => "(ML;OICI;NW;;;ME)",
            (Label::ReadOnly, false) => "(ML;;NW;;;ME)",
            (Label::Hidden, true) => "(ML;OICI;NRNW;;;ME)",
            (Label::Hidden, false) => "(ML;;NRNW;;;ME)",
        }
    }
}

/// The Windows backend that enforces `policy`. Low integrity confines writes only, so Dev with
/// network keeps it; `strict` (which also confines reads) and policies without network run in an
/// AppContainer, which grants nothing outside its listed roots and has no network capability
/// unless one is granted.
#[derive(Debug, PartialEq, Eq)]
enum Backend<'a> {
    LowIntegrity {
        deny_read: &'a [PathBuf],
        extra_write: &'a [PathBuf],
    },
    AppContainer(super::appcontainer::Plan<'a>),
}

fn backend(policy: &Policy) -> Backend<'_> {
    match policy {
        Policy::Dev {
            deny_read,
            extra_write,
            allow_network: true,
        } => Backend::LowIntegrity {
            deny_read,
            extra_write,
        },
        _ => Backend::AppContainer(super::appcontainer::plan(policy)),
    }
}

/// Refuse a writable root inside a protected path: labeling the root would expose the protected
/// contents below it, and labeling the protected path would make the root unwritable.
pub(super) fn check_overlap(writable: &[PathBuf], denied: &[PathBuf]) -> Result<(), String> {
    for root in writable {
        if let Some(deny) = denied.iter().find(|deny| root.starts_with(deny)) {
            return Err(format!(
                "writable path {} lies inside the protected path {}",
                root.display(),
                deny.display()
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
pub use imp::launcher_main;
#[cfg(windows)]
pub(super) use imp::{
    Attribute, CHILD_PROCESS_RESTRICTED, command, environment, probe, spawn, wide,
};

#[cfg(windows)]
mod imp {
    use super::{
        Backend, CHILD_FENCE_ARG, LAUNCH_ARG, Label, backend, check_overlap, command_line,
    };
    use crate::tools::exec::sandbox::{
        Policy, Sandbox, child_env_allowed, expressible, git_metadata, resolve, run_probe_command,
    };
    use base64::Engine as _;
    use std::collections::HashSet;
    use std::ffi::c_void;
    use std::ffi::{OsStr, OsString};
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::ptr::null_mut;
    use std::sync::{LazyLock, Mutex, PoisonError};
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW,
        ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertStringSidToSidW,
        GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
    };
    use windows_sys::Win32::Security::{
        ACL, GetLengthSid, GetSecurityDescriptorSacl, LABEL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, PSID, SID_AND_ATTRIBUTES, SetTokenInformation, TOKEN_ADJUST_DEFAULT,
        TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenIntegrityLevel,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    use windows_sys::Win32::System::SystemServices::SE_GROUP_INTEGRITY;
    use windows_sys::Win32::System::Threading::{
        CREATE_SUSPENDED, CreateProcessW, DeleteProcThreadAttributeList,
        EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetExitCodeProcess, INFINITE,
        InitializeProcThreadAttributeList, OpenProcessToken,
        PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY, PROCESS_INFORMATION, ResumeThread,
        STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
        WaitForSingleObject,
    };

    /// `PROCESS_CREATION_CHILD_PROCESS_RESTRICTED`: the process may not create children.
    pub const CHILD_PROCESS_RESTRICTED: u32 = 1;

    /// The Low mandatory level SID.
    const LOW_INTEGRITY_SID: &str = "S-1-16-4096";

    pub fn wide(text: &OsStr) -> Vec<u16> {
        text.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// Labels applied by this process, so a root is labeled (and walked) at most once.
    static APPLIED: LazyLock<Mutex<HashSet<(PathBuf, Label)>>> =
        LazyLock::new(|| Mutex::new(HashSet::new()));

    /// The SDDL text of `path`'s mandatory label, such as `S:(ML;OICI;NW;;;LW)`.
    fn current_label(path: &Path) -> Result<String, String> {
        let name = wide(path.as_os_str());
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: `name` is NUL-terminated and outlives the call; the out-pointers are valid.
        let status = unsafe {
            GetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                LABEL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
                &mut descriptor,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(format!(
                "reading the integrity label of {}: Windows error {status}",
                path.display()
            ));
        }
        let mut text: *mut u16 = null_mut();
        // SAFETY: `descriptor` came from GetNamedSecurityInfoW; `text` receives a LocalAlloc string.
        let converted = unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor,
                SDDL_REVISION_1,
                LABEL_SECURITY_INFORMATION,
                &mut text,
                null_mut(),
            )
        };
        let result = if converted == 0 || text.is_null() {
            Err(format!(
                "formatting the integrity label of {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ))
        } else {
            let mut len = 0;
            // SAFETY: `text` is a NUL-terminated UTF-16 string allocated by the call above, so
            // every index up to and including the terminator is readable.
            while unsafe { *text.add(len) } != 0 {
                len += 1;
            }
            // SAFETY: the `len` units before the terminator were just read through `text`.
            let units = unsafe { std::slice::from_raw_parts(text, len) };
            Ok(String::from_utf16_lossy(units))
        };
        // SAFETY: both pointers were allocated with LocalAlloc by the calls above (or are null).
        unsafe {
            if !text.is_null() {
                LocalFree(text.cast());
            }
            LocalFree(descriptor);
        }
        result
    }

    /// Replace `path`'s explicit mandatory label; Windows propagates an inheritable label to the
    /// existing contents of a directory.
    fn set_label(path: &Path, ace: &str) -> Result<(), String> {
        let sddl = wide(OsStr::new(&format!("S:{ace}")));
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: `sddl` is NUL-terminated; `descriptor` receives a LocalAlloc buffer.
        let parsed = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                null_mut(),
            )
        };
        if parsed == 0 {
            return Err(format!(
                "building integrity label {ace}: {}",
                std::io::Error::last_os_error()
            ));
        }
        let mut present = 0;
        let mut defaulted = 0;
        let mut sacl: *mut ACL = null_mut();
        // SAFETY: `descriptor` is valid until freed below; the out-pointers are valid.
        let found = unsafe {
            GetSecurityDescriptorSacl(descriptor, &mut present, &mut sacl, &mut defaulted)
        };
        let result = if found == 0 || present == 0 || sacl.is_null() {
            Err(format!("integrity label {ace} has no SACL"))
        } else {
            let name = wide(path.as_os_str());
            // SAFETY: `name` is NUL-terminated; `sacl` points into `descriptor`, still alive.
            let status = unsafe {
                SetNamedSecurityInfoW(
                    name.as_ptr(),
                    SE_FILE_OBJECT,
                    LABEL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    sacl,
                )
            };
            if status == ERROR_SUCCESS {
                Ok(())
            } else {
                Err(format!(
                    "setting the integrity label of {}: Windows error {status}",
                    path.display()
                ))
            }
        };
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe { LocalFree(descriptor) };
        result
    }

    /// Give `path` `label` unless it already carries that explicit label.
    fn ensure(path: &Path, label: Label) -> Result<(), String> {
        let key = (path.to_path_buf(), label);
        if APPLIED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&key)
        {
            return Ok(());
        }
        let ace = label.ace(path.is_dir());
        // An inherited label reads `...;OICIID;...`, so only an explicit one matches.
        if !current_label(path)?.contains(ace) {
            set_label(path, ace)?;
        }
        APPLIED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key);
        Ok(())
    }

    /// Label the writable roots, their Git metadata and the protected paths for one command.
    fn prepare(
        workspace: &Path,
        deny_read: &[PathBuf],
        extra_write: &[PathBuf],
    ) -> Result<(), String> {
        let mut writable = vec![workspace.to_path_buf()];
        for path in extra_write {
            expressible(path)?;
            let path = resolve(path)?;
            if path.is_dir() {
                writable.push(path);
            }
        }
        let mut denied = Vec::new();
        for path in deny_read {
            expressible(path)?;
            let path = resolve(path)?;
            if path.exists() {
                denied.push(path);
            }
        }
        check_overlap(&writable, &denied)?;
        let git = git_metadata(&writable)?;
        for root in &writable {
            ensure(root, Label::Writable)?;
        }
        for path in git.iter().filter(|path| path.exists()) {
            ensure(path, Label::ReadOnly)?;
        }
        for path in &denied {
            ensure(path, Label::Hidden)?;
        }
        Ok(())
    }

    /// Build the launcher command for `program args…` under the Dev `policy`. The environment
    /// is rebuilt from allowlisted variables with `TEMP`/`TMP` at the workspace scratch dir and
    /// toolchain caches pointed below `cache` (which the policy must list as writable).
    pub(in crate::tools::exec::sandbox) fn command(
        workspace: &Path,
        policy: &Policy,
        program: &str,
        args: &[&str],
        env: impl Iterator<Item = (OsString, OsString)>,
        cache: Option<&Path>,
    ) -> Result<Command, String> {
        let (deny_read, extra_write) = match backend(policy) {
            Backend::AppContainer(plan) => {
                return super::super::appcontainer::command(
                    workspace, &plan, program, args, env, cache,
                );
            }
            Backend::LowIntegrity {
                deny_read,
                extra_write,
            } => (deny_read, extra_write),
        };
        prepare(workspace, deny_read, extra_write)?;
        let kept = environment(workspace, extra_write, env, cache)?;
        let exe = std::env::current_exe().map_err(|e| format!("locating Dial: {e}"))?;
        let mut command = Command::new(exe);
        command
            .arg(LAUNCH_ARG)
            .arg(program)
            .args(args)
            .env_clear()
            .envs(kept);
        Ok(command)
    }

    /// The environment of a sandboxed command: allowlisted variables, toolchain caches pointed
    /// below `cache` (which must be writable under the policy) and `TEMP`/`TMP` at the workspace
    /// scratch directory, which this creates.
    pub(in crate::tools::exec::sandbox) fn environment(
        workspace: &Path,
        extra_write: &[PathBuf],
        env: impl Iterator<Item = (OsString, OsString)>,
        cache: Option<&Path>,
    ) -> Result<Vec<(OsString, OsString)>, String> {
        let scratch = workspace.join(crate::workspace_copy::SCRATCH_DIR);
        std::fs::create_dir_all(&scratch)
            .map_err(|e| format!("creating scratch directory: {e}"))?;
        let mut kept: Vec<(OsString, OsString)> = env
            .filter(|(name, value)| match (name.to_str(), value.to_str()) {
                (Some(name), Some(value)) => child_env_allowed(name, value),
                _ => false,
            })
            .collect();
        if let Some(root) = cache {
            let mut writable = vec![workspace.to_path_buf()];
            writable.extend(extra_write.iter().cloned());
            super::super::redirect_toolchain_caches(&mut kept, root, &writable);
        }
        kept.retain(|(name, _)| {
            !name.eq_ignore_ascii_case("TEMP") && !name.eq_ignore_ascii_case("TMP")
        });
        kept.push(("TEMP".into(), scratch.clone().into_os_string()));
        kept.push(("TMP".into(), scratch.into_os_string()));
        Ok(kept)
    }

    /// Lower this process's token to the Low mandatory level; children inherit it.
    fn lower_integrity() -> Result<(), String> {
        let mut token: HANDLE = null_mut();
        // SAFETY: GetCurrentProcess returns a pseudo-handle; `token` receives a real handle.
        if unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_ADJUST_DEFAULT | TOKEN_QUERY,
                &mut token,
            )
        } == 0
        {
            return Err(format!(
                "opening the process token: {}",
                std::io::Error::last_os_error()
            ));
        }
        let sid_text = wide(OsStr::new(LOW_INTEGRITY_SID));
        let mut sid: PSID = null_mut();
        // SAFETY: `sid_text` is NUL-terminated; `sid` receives a LocalAlloc SID.
        let result = if unsafe { ConvertStringSidToSidW(sid_text.as_ptr(), &mut sid) } == 0 {
            Err(format!(
                "building the Low integrity SID: {}",
                std::io::Error::last_os_error()
            ))
        } else {
            let label = TOKEN_MANDATORY_LABEL {
                Label: SID_AND_ATTRIBUTES {
                    Sid: sid,
                    Attributes: SE_GROUP_INTEGRITY as u32,
                },
            };
            // SAFETY: `sid` is a valid SID; `label` and `sid` outlive the call.
            let size =
                std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32 + unsafe { GetLengthSid(sid) };
            // SAFETY: `token` was opened with TOKEN_ADJUST_DEFAULT; `label` is a valid buffer.
            let set = unsafe {
                SetTokenInformation(
                    token,
                    TokenIntegrityLevel,
                    (&label as *const TOKEN_MANDATORY_LABEL).cast(),
                    size,
                )
            };
            if set == 0 {
                Err(format!(
                    "lowering the integrity level: {}",
                    std::io::Error::last_os_error()
                ))
            } else {
                Ok(())
            }
        };
        // SAFETY: `sid` (if set) came from LocalAlloc; `token` is an open handle.
        unsafe {
            if !sid.is_null() {
                LocalFree(sid);
            }
            CloseHandle(token);
        }
        result
    }

    /// One `PROC_THREAD_ATTRIBUTE_*` entry for [`spawn`]; `value` must outlive the call.
    pub struct Attribute {
        pub key: usize,
        pub value: *const c_void,
        pub size: usize,
    }

    /// Start `program args…` suspended with inherited stdio and `attributes`, let `check`
    /// inspect the new process (it is killed if `check` fails), then run it and return its exit
    /// code once it exits.
    pub fn spawn(
        program: &OsStr,
        args: Vec<OsString>,
        attributes: &[Attribute],
        check: &dyn Fn(HANDLE) -> Result<(), String>,
    ) -> Result<i32, String> {
        let program = program.to_str().ok_or("program path is not UTF-8")?;
        let args: Vec<String> = args
            .into_iter()
            .map(|arg| {
                arg.into_string()
                    .map_err(|_| "argument is not UTF-8".to_string())
            })
            .collect::<Result<_, _>>()?;
        let mut line = wide(OsStr::new(&command_line(program, &args)?));
        let count = attributes.len() as u32;
        let mut size = 0usize;
        // SAFETY: a null list only reports the size the attributes need; the call "fails" by design.
        unsafe { InitializeProcThreadAttributeList(null_mut(), count, 0, &mut size) };
        let mut storage = vec![0usize; size.div_ceil(std::mem::size_of::<usize>())];
        let list = storage.as_mut_ptr().cast();
        // SAFETY: `storage` holds at least `size` bytes, aligned for the attribute list.
        if unsafe { InitializeProcThreadAttributeList(list, count, 0, &mut size) } == 0 {
            return Err(format!(
                "preparing process attributes: {}",
                std::io::Error::last_os_error()
            ));
        }
        let result = (|| {
            for attribute in attributes {
                // SAFETY: `list` is initialized for `count` attributes; the caller keeps
                // `value` alive until this function returns.
                if unsafe {
                    UpdateProcThreadAttribute(
                        list,
                        0,
                        attribute.key,
                        attribute.value,
                        attribute.size,
                        null_mut(),
                        std::ptr::null(),
                    )
                } == 0
                {
                    return Err(format!(
                        "setting process attribute {:#x}: {}",
                        attribute.key,
                        std::io::Error::last_os_error()
                    ));
                }
            }
            // SAFETY: all-zero is a valid empty STARTUPINFOEXW/PROCESS_INFORMATION.
            let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
            startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
            startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            // SAFETY: GetStdHandle only reads this process's standard handles.
            unsafe {
                startup.StartupInfo.hStdInput = GetStdHandle(STD_INPUT_HANDLE);
                startup.StartupInfo.hStdOutput = GetStdHandle(STD_OUTPUT_HANDLE);
                startup.StartupInfo.hStdError = GetStdHandle(STD_ERROR_HANDLE);
            }
            startup.lpAttributeList = list;
            // SAFETY: as above.
            let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: `line` is a mutable NUL-terminated buffer; `startup` and its attribute list
            // stay alive for the call; the inherited standard handles were inherited by us.
            let created = unsafe {
                CreateProcessW(
                    std::ptr::null(),
                    line.as_mut_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    1,
                    EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED,
                    std::ptr::null(),
                    std::ptr::null(),
                    &startup.StartupInfo,
                    &mut info,
                )
            };
            if created == 0 {
                return Err(format!(
                    "starting {program}: {}",
                    std::io::Error::last_os_error()
                ));
            }
            if let Err(error) = check(info.hProcess) {
                // SAFETY: both handles are open and owned here; the process never ran.
                unsafe {
                    TerminateProcess(info.hProcess, 126);
                    CloseHandle(info.hThread);
                    CloseHandle(info.hProcess);
                }
                return Err(error);
            }
            let mut code = 1u32;
            // SAFETY: both handles are open and owned here; each is closed exactly once.
            unsafe {
                ResumeThread(info.hThread);
                CloseHandle(info.hThread);
                WaitForSingleObject(info.hProcess, INFINITE);
                GetExitCodeProcess(info.hProcess, &mut code);
                CloseHandle(info.hProcess);
            }
            Ok(code as i32)
        })();
        // SAFETY: `list` was initialized above and is no longer used.
        unsafe { DeleteProcThreadAttributeList(list) };
        result
    }

    /// Run `program args…` with child-process creation denied and inherited stdio; return its
    /// exit code once it exits.
    fn run_fenced(program: &OsStr, args: Vec<OsString>) -> Result<i32, String> {
        let policy: u32 = CHILD_PROCESS_RESTRICTED;
        let fence = Attribute {
            key: PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY as usize,
            value: (&policy as *const u32).cast(),
            size: std::mem::size_of::<u32>(),
        };
        spawn(program, args, &[fence], &|_| Ok(()))
    }

    /// Entry point for `dial [--dial-low-integrity] [--dial-child-fence] <program> [args…]`:
    /// lower the integrity level and/or deny child processes, run the program with inherited
    /// stdio, and exit with its status.
    pub fn launcher_main() -> i32 {
        let mut args = std::env::args_os().skip(1).peekable();
        if args
            .next_if(|arg| arg == super::super::appcontainer::LAUNCH_ARG)
            .is_some()
        {
            return super::super::appcontainer::launcher_main(args);
        }
        let lower = args.next_if(|arg| arg == LAUNCH_ARG).is_some();
        let fence = args.next_if(|arg| arg == CHILD_FENCE_ARG).is_some();
        let Some(program) = args.next() else {
            eprintln!("dial: {LAUNCH_ARG}/{CHILD_FENCE_ARG} requires a program");
            return 126;
        };
        if lower && let Err(error) = lower_integrity() {
            eprintln!("dial sandbox: {error}");
            return 126;
        }
        if fence {
            return match run_fenced(&program, args.collect()) {
                Ok(code) => code,
                Err(error) => {
                    eprintln!("dial sandbox: {error}");
                    127
                }
            };
        }
        match Command::new(&program).args(args).status() {
            Ok(status) => status.code().unwrap_or(1),
            Err(error) => {
                eprintln!(
                    "dial sandbox: starting {}: {error}",
                    Path::new(&program).display()
                );
                127
            }
        }
    }

    fn powershell() -> Result<PathBuf, String> {
        let root = std::env::var_os("SystemRoot").ok_or("SystemRoot is unavailable")?;
        Ok(Path::new(&root)
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe"))
    }

    const SECRET: &str = "dial-low-integrity-probe-secret";

    /// Fixed enforcement test, run through the real launcher and PowerShell. Every attempt is
    /// made inside the command; the outcome is judged here from the files and the output.
    fn self_test() -> Result<(), String> {
        let scratch = tempfile::tempdir().map_err(|e| format!("creating scratch dir: {e}"))?;
        let base = resolve(scratch.path())?;
        let workspace = base.join("ws");
        let outside = base.join("outside");
        let denied = base.join("denied");
        let git = workspace.join(".git");
        for dir in [&workspace, &outside, &denied, &git] {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let write =
            |path: PathBuf, text: &str| std::fs::write(path, text).map_err(|e| e.to_string());
        write(denied.join("secret.txt"), SECRET)?;
        write(git.join("config"), "original")?;
        write(git.join("HEAD"), "ref: refs/heads/main\n")?;

        let policy = Policy::Dev {
            deny_read: vec![denied.clone()],
            extra_write: Vec::new(),
            allow_network: true,
        };
        let script = r"
$ErrorActionPreference = 'SilentlyContinue'
Set-Content -Path probe.txt -Value ok -NoNewline
New-Item -ItemType Directory -Path made | Out-Null
Remove-Item -Path made -Force
Set-Content -Path ..\outside\canary.txt -Value x
Set-Content -Path .git\config -Value changed
Remove-Item -Path .git\HEAD -Force
Get-Content -Path ..\denied\secret.txt
whoami /groups
";
        let encoded: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let encoded = base64::engine::general_purpose::STANDARD.encode(encoded);
        let powershell = powershell()?;
        let powershell = powershell.to_str().ok_or("PowerShell path is not UTF-8")?;
        let mut command = command(
            &workspace,
            &policy,
            powershell,
            &[
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-EncodedCommand",
                &encoded,
            ],
            std::env::vars_os(),
            None,
        )?;
        command
            .current_dir(&workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let (code, stdout) = run_probe_command(command)?;
        if code == Some(126) || code == Some(127) {
            return Err(format!(
                "the launcher could not start PowerShell (exit {code:?})"
            ));
        }
        if !stdout.contains(LOW_INTEGRITY_SID) {
            return Err("the command did not run at Low integrity".into());
        }
        if std::fs::read_to_string(workspace.join("probe.txt"))
            .ok()
            .as_deref()
            != Some("ok")
        {
            return Err("writing inside the workspace failed".into());
        }
        if workspace.join("made").exists() {
            return Err("deleting inside the workspace failed".into());
        }
        if outside.join("canary.txt").exists() {
            return Err("a write outside the workspace was not blocked".into());
        }
        if stdout.contains(SECRET) {
            return Err("a protected path was readable".into());
        }
        if std::fs::read_to_string(git.join("config")).ok().as_deref() != Some("original")
            || !git.join("HEAD").exists()
        {
            return Err("Git metadata inside the workspace was writable".into());
        }
        Ok(())
    }

    /// Run the enforcement test once per process.
    pub(in crate::tools::exec::sandbox) fn probe() -> Sandbox {
        static DETECTED: LazyLock<Sandbox> = LazyLock::new(|| match self_test() {
            Ok(()) => Sandbox::Available,
            Err(reason) => {
                Sandbox::Unavailable(format!("Windows Low-integrity self-test failed: {reason}"))
            }
        });
        DETECTED.clone()
    }
}

#[cfg(test)]
mod tests;
