//! Windows AppContainer confinement for the policies Low integrity cannot enforce: `strict`, and
//! `dev` with `allow_network = false`.
//!
//! An AppContainer process can open only objects whose ACL grants its AppContainer SID (or
//! `ALL APPLICATION PACKAGES`, which Windows grants on the system and program directories). It has
//! no network unless the token carries a network capability. So:
//!
//! * each workspace gets its own AppContainer profile (`dial.sandbox.<hash>`), and the workspace
//!   and each `extra_write` root grant that SID full access, inherited by their contents;
//! * reads elsewhere in the user profile and on other drives are refused because nothing grants
//!   them, and each existing `deny_read` path gets an explicit deny entry, so protected paths
//!   inside a writable root stay closed;
//! * existing Git metadata inside a writable root gets an explicit deny-write entry;
//! * the token has no capabilities when the policy has no network, and `internetClient` plus
//!   `privateNetworkClientServer` when `strict` allows network. Windows never lets an
//!   AppContainer reach loopback, so local servers are unreachable either way.
//!
//! Dial re-executes itself with [`LAUNCH_ARG`]; that launcher (an ordinary process) creates the
//! program suspended with the AppContainer security capabilities, checks that the new token is an
//! AppContainer token with exactly the expected capabilities, and only then lets it run. [`probe`]
//! runs a fixed PowerShell enforcement test through the real launcher; any failed check makes the
//! sandbox unavailable rather than unconfined.
//!
//! The access entries are stored in the folders' security descriptors and stay after Dial exits;
//! they name only this workspace's AppContainer, so ordinary programs are unaffected.
//!
//! Residual limits:
//! * Programs the container cannot read (installed under the user profile, such as a per-user
//!   Node or Python) cannot run unless their folder is listed in `sandbox.extra_write`.
//! * As with Low integrity, a protected `.git` can be renamed away through the parent's
//!   delete-child right; in-place changes are refused.
//! * Domain filtering (`network_allow`/`network_deny`) is still refused on Windows.

// Compiled on other platforms only for the policy tests below.
#![cfg_attr(not(windows), allow(dead_code))]

use super::Policy;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Hidden first argument that turns a Dial process into the AppContainer launcher.
pub const LAUNCH_ARG: &str = "--dial-appcontainer";

/// Launcher argument: grant the network capabilities.
const NETWORK_ARG: &str = "--dial-network";

/// Profile names Dial creates; the launcher refuses any other.
const PROFILE_PREFIX: &str = "dial.sandbox.";

/// Capability SIDs: `internetClient` and `privateNetworkClientServer`.
const NETWORK_CAPABILITIES: [&str; 2] = ["S-1-15-3-1", "S-1-15-3-3"];

/// What the AppContainer backend enforces for one policy.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Plan<'a> {
    pub deny_read: &'a [PathBuf],
    pub extra_write: &'a [PathBuf],
    pub network: bool,
}

pub(super) fn plan(policy: &Policy) -> Plan<'_> {
    match policy {
        Policy::Strict {
            deny_read,
            allow_network,
            extra_write,
        }
        | Policy::Dev {
            deny_read,
            extra_write,
            allow_network,
        } => Plan {
            deny_read,
            extra_write,
            network: *allow_network,
        },
    }
}

/// The capability SIDs the container token carries.
fn capabilities(network: bool) -> &'static [&'static str] {
    if network { &NETWORK_CAPABILITIES } else { &[] }
}

/// The AppContainer profile for `workspace`: stable per path (case-insensitive, as Windows
/// paths are), at most 64 characters, so one project's grants never open another's files.
pub(super) fn profile_name(workspace: &Path) -> String {
    use sha2::{Digest, Sha256};
    let key = workspace.to_string_lossy().to_lowercase();
    let digest = Sha256::digest(key.as_bytes());
    let hex: String = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{PROFILE_PREFIX}{hex}")
}

/// Parsed launcher arguments after [`LAUNCH_ARG`]:
/// `<profile> [--dial-network] [--dial-child-fence] <program> [args…]`.
#[derive(Debug, PartialEq, Eq)]
struct Launch {
    profile: String,
    network: bool,
    fence: bool,
    program: OsString,
    args: Vec<OsString>,
}

fn parse(args: impl Iterator<Item = OsString>) -> Result<Launch, String> {
    let mut args = args.peekable();
    let profile = args
        .next()
        .and_then(|profile| profile.into_string().ok())
        .filter(|profile| {
            profile.len() <= 64
                && profile.strip_prefix(PROFILE_PREFIX).is_some_and(|hash| {
                    !hash.is_empty() && hash.bytes().all(|b| b.is_ascii_hexdigit())
                })
        })
        .ok_or_else(|| format!("{LAUNCH_ARG} requires a Dial sandbox profile name"))?;
    let network = args.next_if(|arg| arg == NETWORK_ARG).is_some();
    let fence = args
        .next_if(|arg| arg == super::low_integrity::CHILD_FENCE_ARG)
        .is_some();
    let program = args
        .next()
        .ok_or_else(|| format!("{LAUNCH_ARG} requires a program"))?;
    Ok(Launch {
        profile,
        network,
        fence,
        program,
        args: args.collect(),
    })
}

#[cfg(all(windows, test))]
pub(super) use imp::probe;
#[cfg(windows)]
pub(super) use imp::{command, launcher_main};

#[cfg(windows)]
mod imp {
    use super::{LAUNCH_ARG, Launch, NETWORK_ARG, Plan, capabilities, parse, profile_name};
    use crate::tools::exec::sandbox::low_integrity::{
        Attribute, CHILD_PROCESS_RESTRICTED, environment, spawn, wide,
    };
    use crate::tools::exec::sandbox::{
        Policy, Sandbox, expressible, git_metadata, resolve, run_probe_command,
    };
    use std::collections::HashSet;
    use std::ffi::{OsStr, OsString};
    use std::os::windows::ffi::OsStringExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::ptr::null_mut;
    use std::sync::{LazyLock, Mutex, PoisonError};
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
        ConvertStringSidToSidW, DENY_ACCESS, EXPLICIT_ACCESS_W, GRANT_ACCESS,
        GetNamedSecurityInfoW, NO_MULTIPLE_TRUSTEE, SDDL_REVISION_1, SE_FILE_OBJECT,
        SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_WELL_KNOWN_GROUP,
        TRUSTEE_W,
    };
    use windows_sys::Win32::Security::Isolation::{
        CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
    };
    use windows_sys::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, FreeSid, GetTokenInformation, NO_INHERITANCE,
        PSECURITY_DESCRIPTOR, PSID, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES,
        SUB_CONTAINERS_AND_OBJECTS_INHERIT, TOKEN_GROUPS, TOKEN_QUERY, TokenCapabilities,
        TokenIsAppContainer,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ALL_ACCESS, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ,
    };
    use windows_sys::Win32::System::SystemServices::SE_GROUP_ENABLED;
    use windows_sys::Win32::System::Threading::{
        OpenProcessToken, PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY,
        PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
    };

    /// `HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS)`.
    const ALREADY_EXISTS: i32 = 0x8007_00B7_u32 as i32;

    /// Every right that changes a file or directory, its attributes, its security descriptor or
    /// its children: write/append data, write EA and attributes, delete, delete child, WRITE_DAC
    /// and WRITE_OWNER. Read, execute and SYNCHRONIZE stay available.
    const WRITE_RIGHTS: u32 = 0x2 | 0x4 | 0x10 | 0x100 | 0x1_0000 | 0x40 | 0x4_0000 | 0x8_0000;

    /// The fixed profile of the enforcement self-test, so it does not create one per run.
    const SELF_TEST_PROFILE: &str = "dial.sandbox.5e1f7e57";

    /// A SID released with `FreeSid` (AppContainer SIDs) or `LocalFree` (string conversions).
    struct Sid {
        sid: PSID,
        local: bool,
    }

    impl Drop for Sid {
        fn drop(&mut self) {
            // SAFETY: `sid` came from the matching allocator and is freed exactly once.
            unsafe {
                if self.local {
                    LocalFree(self.sid);
                } else {
                    FreeSid(self.sid);
                }
            }
        }
    }

    fn os_error(what: &str) -> String {
        format!("{what}: {}", std::io::Error::last_os_error())
    }

    /// The AppContainer SID of `name`, creating the profile first when `create` is set.
    fn container_sid(name: &str, create: bool) -> Result<Sid, String> {
        let text = wide(OsStr::new(name));
        let mut sid: PSID = null_mut();
        if create {
            // SAFETY: the strings are NUL-terminated; no capabilities; `sid` receives a SID.
            let result = unsafe {
                CreateAppContainerProfile(
                    text.as_ptr(),
                    text.as_ptr(),
                    text.as_ptr(),
                    std::ptr::null(),
                    0,
                    &mut sid,
                )
            };
            if result >= 0 {
                return Ok(Sid { sid, local: false });
            }
            if result != ALREADY_EXISTS {
                return Err(format!(
                    "creating the AppContainer profile {name}: HRESULT {result:#010x}"
                ));
            }
        }
        // SAFETY: `text` is NUL-terminated; `sid` receives a SID.
        let result = unsafe { DeriveAppContainerSidFromAppContainerName(text.as_ptr(), &mut sid) };
        if result < 0 {
            return Err(format!(
                "deriving the AppContainer SID of {name}: HRESULT {result:#010x}"
            ));
        }
        Ok(Sid { sid, local: false })
    }

    fn string_sid(text: &str) -> Result<Sid, String> {
        let text = wide(OsStr::new(text));
        let mut sid: PSID = null_mut();
        // SAFETY: `text` is NUL-terminated; `sid` receives a LocalAlloc SID.
        if unsafe { ConvertStringSidToSidW(text.as_ptr(), &mut sid) } == 0 {
            return Err(os_error("building a capability SID"));
        }
        Ok(Sid { sid, local: true })
    }

    /// Read and free a LocalAlloc'd NUL-terminated wide string.
    ///
    /// # Safety
    /// `text` must be a valid NUL-terminated string from LocalAlloc, not used afterwards.
    unsafe fn take_wide(text: *mut u16) -> String {
        let mut length = 0;
        // SAFETY: the caller guarantees NUL termination.
        while unsafe { *text.add(length) } != 0 {
            length += 1;
        }
        // SAFETY: `length` elements were just read.
        let slice = unsafe { std::slice::from_raw_parts(text, length) };
        let value = OsString::from_wide(slice).to_string_lossy().into_owned();
        // SAFETY: allocated with LocalAlloc by the API that returned it.
        unsafe { LocalFree(text.cast()) };
        value
    }

    fn sid_text(sid: &Sid) -> Result<String, String> {
        let mut text = null_mut();
        // SAFETY: `sid` is valid; `text` receives a LocalAlloc string.
        if unsafe { ConvertSidToStringSidW(sid.sid, &mut text) } == 0 {
            return Err(os_error("formatting the AppContainer SID"));
        }
        // SAFETY: returned by ConvertSidToStringSidW.
        Ok(unsafe { take_wide(text) })
    }

    /// The access one path gives the container.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    enum Access {
        /// Writable root: everything.
        Full,
        /// Dial itself: read and execute, so the launcher's program may be Dial.
        ReadExecute,
        /// Git metadata: readable, never changed in place.
        NoWrite,
        /// Protected path: no access at all.
        None,
    }

    impl Access {
        fn entry(self) -> (i32, u32) {
            match self {
                Access::Full => (GRANT_ACCESS, FILE_ALL_ACCESS),
                Access::ReadExecute => (GRANT_ACCESS, FILE_GENERIC_READ | FILE_GENERIC_EXECUTE),
                Access::NoWrite => (DENY_ACCESS, WRITE_RIGHTS),
                Access::None => (DENY_ACCESS, FILE_ALL_ACCESS),
            }
        }

        /// The SDDL ACE type letter of the entry.
        fn kind(self) -> &'static str {
            match self {
                Access::Full | Access::ReadExecute => "A",
                Access::NoWrite | Access::None => "D",
            }
        }
    }

    /// Entries applied by this process, so a root is changed (and walked) at most once.
    static APPLIED: LazyLock<Mutex<HashSet<(PathBuf, String, Access)>>> =
        LazyLock::new(|| Mutex::new(HashSet::new()));

    /// Whether the DACL already holds a direct (not inherited) entry of this kind for `sid`.
    fn has_entry(descriptor: PSECURITY_DESCRIPTOR, sid: &str, access: Access) -> bool {
        let mut text = null_mut();
        // SAFETY: `descriptor` is valid; `text` receives a LocalAlloc string.
        if unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut text,
                null_mut(),
            )
        } == 0
        {
            return false;
        }
        // SAFETY: returned by the conversion above.
        let sddl = unsafe { take_wide(text) };
        let suffix = format!(";;;{sid}");
        sddl.split(['(', ')']).any(|ace| {
            let fields: Vec<&str> = ace.splitn(3, ';').collect();
            ace.ends_with(&suffix)
                && fields.len() == 3
                && fields[0] == access.kind()
                && !fields[1].contains("ID")
        })
    }

    fn apply(path: &Path, sid: &Sid, text: &str, access: Access) -> Result<(), String> {
        let key = (path.to_path_buf(), text.to_owned(), access);
        if APPLIED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&key)
        {
            return Ok(());
        }
        let name = wide(path.as_os_str());
        let mut dacl: *mut ACL = null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: `name` is NUL-terminated; the outputs receive a LocalAlloc descriptor and a
        // pointer into it.
        let error = unsafe {
            GetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                &mut dacl,
                null_mut(),
                &mut descriptor,
            )
        };
        if error != ERROR_SUCCESS {
            return Err(format!(
                "reading the access list of {}: {}",
                path.display(),
                std::io::Error::from_raw_os_error(error as i32)
            ));
        }
        let result = if has_entry(descriptor, text, access) {
            Ok(())
        } else {
            let (mode, rights) = access.entry();
            let entry = EXPLICIT_ACCESS_W {
                grfAccessPermissions: rights,
                grfAccessMode: mode,
                grfInheritance: if path.is_dir() {
                    SUB_CONTAINERS_AND_OBJECTS_INHERIT
                } else {
                    NO_INHERITANCE
                },
                Trustee: TRUSTEE_W {
                    pMultipleTrustee: null_mut(),
                    MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                    TrusteeForm: TRUSTEE_IS_SID,
                    TrusteeType: TRUSTEE_IS_WELL_KNOWN_GROUP,
                    ptstrName: sid.sid.cast(),
                },
            };
            let mut updated: *mut ACL = null_mut();
            // SAFETY: `entry` names a valid SID; `dacl` points into `descriptor`; `updated`
            // receives a LocalAlloc ACL.
            let error = unsafe { SetEntriesInAclW(1, &entry, dacl, &mut updated) };
            if error != ERROR_SUCCESS {
                Err(format!(
                    "building the access list of {}: {}",
                    path.display(),
                    std::io::Error::from_raw_os_error(error as i32)
                ))
            } else {
                // SAFETY: `name` is NUL-terminated; `updated` is a valid ACL.
                let error = unsafe {
                    SetNamedSecurityInfoW(
                        name.as_ptr(),
                        SE_FILE_OBJECT,
                        DACL_SECURITY_INFORMATION,
                        null_mut(),
                        null_mut(),
                        updated,
                        std::ptr::null(),
                    )
                };
                // SAFETY: allocated by SetEntriesInAclW.
                unsafe { LocalFree(updated.cast()) };
                if error == ERROR_SUCCESS {
                    Ok(())
                } else {
                    Err(format!(
                        "setting the access list of {}: {}",
                        path.display(),
                        std::io::Error::from_raw_os_error(error as i32)
                    ))
                }
            }
        };
        // SAFETY: allocated by GetNamedSecurityInfoW; `dacl` is not used afterwards.
        unsafe { LocalFree(descriptor) };
        if result.is_ok() {
            APPLIED
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(key);
        }
        result
    }

    /// Grant the writable roots and Dial itself, and close Git metadata and protected paths.
    fn prepare(workspace: &Path, plan: &Plan<'_>, sid: &Sid) -> Result<(), String> {
        let text = sid_text(sid)?;
        let mut writable = vec![workspace.to_path_buf()];
        for path in plan.extra_write {
            expressible(path)?;
            let path = resolve(path)?;
            if path.is_dir() {
                writable.push(path);
            }
        }
        let mut denied = Vec::new();
        for path in plan.deny_read {
            expressible(path)?;
            let path = resolve(path)?;
            if path.exists() {
                denied.push(path);
            }
        }
        super::super::low_integrity::check_overlap(&writable, &denied)?;
        let git = git_metadata(&writable)?;
        for root in &writable {
            apply(root, sid, &text, Access::Full)?;
        }
        for path in git.iter().filter(|path| path.exists()) {
            apply(path, sid, &text, Access::NoWrite)?;
        }
        for path in &denied {
            apply(path, sid, &text, Access::None)?;
        }
        let exe = std::env::current_exe().map_err(|e| format!("locating Dial: {e}"))?;
        apply(&exe, sid, &text, Access::ReadExecute)
    }

    fn build(
        profile: &str,
        workspace: &Path,
        plan: &Plan<'_>,
        program: &str,
        args: &[&str],
        env: impl Iterator<Item = (OsString, OsString)>,
        cache: Option<&Path>,
    ) -> Result<Command, String> {
        let sid = container_sid(profile, true)?;
        prepare(workspace, plan, &sid)?;
        let kept = environment(workspace, plan.extra_write, env, cache)?;
        let exe = std::env::current_exe().map_err(|e| format!("locating Dial: {e}"))?;
        let mut command = Command::new(exe);
        command.arg(LAUNCH_ARG).arg(profile);
        if plan.network {
            command.arg(NETWORK_ARG);
        }
        command.arg(program).args(args).env_clear().envs(kept);
        Ok(command)
    }

    /// The launcher command for `program args…` in `workspace`'s AppContainer under `plan`.
    pub(in crate::tools::exec::sandbox) fn command(
        workspace: &Path,
        plan: &Plan<'_>,
        program: &str,
        args: &[&str],
        env: impl Iterator<Item = (OsString, OsString)>,
        cache: Option<&Path>,
    ) -> Result<Command, String> {
        if let Sandbox::Unavailable(reason) = probe() {
            return Err(reason);
        }
        build(
            &profile_name(workspace),
            workspace,
            plan,
            program,
            args,
            env,
            cache,
        )
    }

    /// Refuse to run unless the suspended process is an AppContainer process carrying exactly
    /// `expected` capabilities.
    fn verify(process: HANDLE, expected: usize) -> Result<(), String> {
        let mut token: HANDLE = null_mut();
        // SAFETY: `process` is an open process handle; `token` receives a handle.
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
            return Err(os_error("opening the sandboxed token"));
        }
        let result = (|| {
            let mut contained = 0u32;
            let mut length = 0u32;
            // SAFETY: `contained` is a 4-byte buffer, as TokenIsAppContainer requires.
            if unsafe {
                GetTokenInformation(
                    token,
                    TokenIsAppContainer,
                    (&mut contained as *mut u32).cast(),
                    4,
                    &mut length,
                )
            } == 0
            {
                return Err(os_error("reading the sandboxed token"));
            }
            if contained == 0 {
                return Err("the command did not start in an AppContainer".into());
            }
            // SAFETY: a null buffer only reports the size needed.
            unsafe { GetTokenInformation(token, TokenCapabilities, null_mut(), 0, &mut length) };
            let mut buffer = vec![0u64; (length as usize).div_ceil(8).max(1)];
            // SAFETY: `buffer` holds at least `length` bytes, aligned for TOKEN_GROUPS.
            if unsafe {
                GetTokenInformation(
                    token,
                    TokenCapabilities,
                    buffer.as_mut_ptr().cast(),
                    (buffer.len() * 8) as u32,
                    &mut length,
                )
            } == 0
            {
                return Err(os_error("reading the sandboxed token's capabilities"));
            }
            // SAFETY: the call above filled a TOKEN_GROUPS header.
            let count = unsafe { (*buffer.as_ptr().cast::<TOKEN_GROUPS>()).GroupCount };
            if count as usize != expected {
                return Err(format!(
                    "the sandboxed token has {count} capabilities, expected {expected}"
                ));
            }
            Ok(())
        })();
        // SAFETY: `token` is open and owned here.
        unsafe { CloseHandle(token) };
        result
    }

    fn run(launch: Launch) -> Result<i32, String> {
        let container = container_sid(&launch.profile, false)?;
        let granted: Vec<Sid> = capabilities(launch.network)
            .iter()
            .map(|text| string_sid(text))
            .collect::<Result<_, _>>()?;
        let mut entries: Vec<SID_AND_ATTRIBUTES> = granted
            .iter()
            .map(|sid| SID_AND_ATTRIBUTES {
                Sid: sid.sid,
                Attributes: SE_GROUP_ENABLED as u32,
            })
            .collect();
        let security = SECURITY_CAPABILITIES {
            AppContainerSid: container.sid,
            Capabilities: if entries.is_empty() {
                null_mut()
            } else {
                entries.as_mut_ptr()
            },
            CapabilityCount: entries.len() as u32,
            Reserved: 0,
        };
        let policy: u32 = CHILD_PROCESS_RESTRICTED;
        let mut attributes = vec![Attribute {
            key: PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
            value: (&security as *const SECURITY_CAPABILITIES).cast(),
            size: std::mem::size_of::<SECURITY_CAPABILITIES>(),
        }];
        if launch.fence {
            attributes.push(Attribute {
                key: PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY as usize,
                value: (&policy as *const u32).cast(),
                size: std::mem::size_of::<u32>(),
            });
        }
        let expected = entries.len();
        spawn(&launch.program, launch.args, &attributes, &|process| {
            verify(process, expected)
        })
    }

    /// Entry point for `dial --dial-appcontainer <profile> [--dial-network]
    /// [--dial-child-fence] <program> [args…]`.
    pub fn launcher_main(args: impl Iterator<Item = OsString>) -> i32 {
        match parse(args).and_then(run) {
            Ok(code) => code,
            Err(error) => {
                eprintln!("dial sandbox: {error}");
                126
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

    const SECRET: &str = "dial-appcontainer-probe-secret";

    /// Fixed enforcement test through the real launcher and PowerShell, without network. Every
    /// attempt is made inside the command; the outcome is judged here from files and output.
    fn self_test() -> Result<(), String> {
        use base64::Engine as _;
        let scratch = tempfile::tempdir().map_err(|e| format!("creating scratch dir: {e}"))?;
        let base = resolve(scratch.path())?;
        let workspace = base.join("ws");
        let outside = base.join("outside");
        let denied = workspace.join("secret");
        let git = workspace.join(".git");
        for dir in [&workspace, &outside, &denied, &git] {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let write =
            |path: PathBuf, text: &str| std::fs::write(path, text).map_err(|e| e.to_string());
        write(denied.join("secret.txt"), SECRET)?;
        write(outside.join("readable.txt"), SECRET)?;
        write(git.join("config"), "original")?;
        let policy = Policy::Strict {
            deny_read: vec![denied.clone()],
            allow_network: false,
            extra_write: Vec::new(),
        };
        let script = r"
$ErrorActionPreference = 'SilentlyContinue'
Set-Content -Path probe.txt -Value ok -NoNewline
New-Item -ItemType Directory -Path made | Out-Null
Remove-Item -Path made -Force
Set-Content -Path ..\outside\canary.txt -Value x
Get-Content -Path ..\outside\readable.txt
Get-Content -Path secret\secret.txt
Set-Content -Path .git\config -Value changed
Write-Output done
";
        let encoded: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let encoded = base64::engine::general_purpose::STANDARD.encode(encoded);
        let powershell = powershell()?;
        let powershell = powershell.to_str().ok_or("PowerShell path is not UTF-8")?;
        let mut command = build(
            SELF_TEST_PROFILE,
            &workspace,
            &super::plan(&policy),
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
        if code == Some(126) || code == Some(127) || !stdout.contains("done") {
            return Err(format!(
                "the launcher could not run PowerShell in an AppContainer (exit {code:?})"
            ));
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
            return Err(
                "a read outside the workspace or of a protected path was not blocked".into(),
            );
        }
        if std::fs::read_to_string(git.join("config")).ok().as_deref() != Some("original") {
            return Err("Git metadata inside the workspace was writable".into());
        }
        Ok(())
    }

    /// Run the enforcement test once per process.
    pub(in crate::tools::exec::sandbox) fn probe() -> Sandbox {
        static DETECTED: LazyLock<Sandbox> = LazyLock::new(|| match self_test() {
            Ok(()) => Sandbox::Available,
            Err(reason) => {
                Sandbox::Unavailable(format!("Windows AppContainer self-test failed: {reason}"))
            }
        });
        DETECTED.clone()
    }
}

#[cfg(test)]
mod tests;
