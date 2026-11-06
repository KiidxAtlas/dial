//! Mandatory Linux eval process fence. Bubblewrap installs it after its own namespace forks;
//! explicit filesystem opt-out installs the identical filter in the child before exec.
//! Threads share a thread group and cannot detach from process-group cancellation. clone3's
//! pointer arguments cannot be inspected by classic BPF, so ENOSYS requests libc's clone fallback.
use std::fs::File;
use std::io::{self, Seek, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::LazyLock;

const ALLOW: u32 = 0x7fff_0000;
const KILL: u32 = 0x8000_0000;
const ERRNO: u32 = 0x0005_0000;

fn stmt(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}
fn jump(k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter {
        code: 0x15,
        jt,
        jf,
        k,
    }
}

fn filter() -> Result<Vec<libc::sock_filter>, String> {
    // Only native 64-bit ABIs are supported. A compat exec is killed on its first syscall.
    let (arch, clone, clone3, denied): (u32, u32, u32, &[u32]) =
        if cfg!(all(target_arch = "x86_64", target_endian = "little")) {
            (0xc000_003e, 56, 435, &[57, 58, 109, 112])
        } else if cfg!(all(target_arch = "aarch64", target_endian = "little")) {
            (0xc000_00b7, 220, 435, &[154, 157])
        } else {
            return Err(
                "Linux eval process-fork containment supports only x86_64 and aarch64".into(),
            );
        };
    let mut code = vec![
        stmt(0x20, 4), // seccomp_data.arch
        jump(arch, 1, 0),
        stmt(0x06, KILL),
        stmt(0x20, 0), // seccomp_data.nr
    ];
    // x32 shares AUDIT_ARCH_X86_64, but has a distinct syscall-number bit. Never let it bypass
    // the native syscall comparisons (also rejects invalid/negative syscall numbers).
    code.extend([
        libc::sock_filter {
            code: 0x35,
            jt: 0,
            jf: 1,
            k: 0x4000_0000,
        },
        stmt(0x06, KILL),
    ]);
    // fork/vfork and changing process group/session are forbidden. A process that joins
    // another group or creates a new session could otherwise escape group-owned cancellation.
    // std's process_group setup runs before our pre_exec hook; bubblewrap installs after its
    // own session setup, and the trusted network bridge installs only in its final child.
    for nr in denied {
        code.extend([jump(*nr, 0, 1), stmt(0x06, ERRNO | libc::EPERM as u32)]);
    }
    code.extend([
        jump(clone3, 0, 1),
        stmt(0x06, ERRNO | libc::ENOSYS as u32),
        jump(clone, 1, 0),
        stmt(0x06, ALLOW),
        stmt(0x20, 16), // low word of seccomp_data.args[0] (native little-endian ABIs)
        // CLONE_VM | CLONE_SIGHAND | CLONE_THREAD are all required. No exit signal or
        // namespace/vfork flags: only the documented pthread flags may accompany them.
        stmt(0x54, !0x017d_0f00u32),
        jump(0, 1, 0),
        stmt(0x06, ERRNO | libc::EPERM as u32),
        stmt(0x20, 16),
        stmt(0x54, 0x0001_0900),
        jump(0x0001_0900, 1, 0),
        stmt(0x06, ERRNO | libc::EPERM as u32),
        stmt(0x20, 20), // high word: reject flag truncation or future extensions
        jump(0, 1, 0),
        stmt(0x06, ERRNO | libc::EPERM as u32),
        stmt(0x06, ALLOW),
    ]);
    Ok(code)
}

fn install(code: &[libc::sock_filter]) -> io::Result<()> {
    let program = libc::sock_fprog {
        len: code.len() as u16,
        filter: code.as_ptr().cast_mut(),
    };
    // SAFETY: prctl scalar arguments follow the kernel ABI; sock_fprog points at a live,
    // initialized BPF instruction slice for the synchronous kernel copy. No Rust memory is
    // modified. Installation is only called in a freshly spawned child, never the controller.
    unsafe {
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
            || libc::prctl(libc::PR_SET_SECCOMP, 2, &program) != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

pub(in crate::tools::exec) fn direct(command: &mut Command) -> Result<(), String> {
    let code = filter()?;
    // SAFETY: pre_exec only invokes kernel prctl with preallocated data. No locks,
    // allocation or non-async-signal-safe library operations run in the forked child.
    unsafe {
        command.pre_exec(move || install(&code));
    }
    Ok(())
}

pub(in crate::tools::exec) fn bubblewrap(command: &mut Command) -> Result<(), String> {
    let code = filter()?;
    // Private anonymous, close-on-exec descriptor; never placed in the writable workspace.
    // SAFETY: static NUL-terminated name; memfd_create returns a new owned fd or -1.
    let fd = unsafe { libc::memfd_create(c"dial-eval-seccomp".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(format!(
            "creating eval seccomp filter: {}",
            io::Error::last_os_error()
        ));
    }
    // SAFETY: fd was just created and has exactly one owner.
    let mut file = unsafe { File::from_raw_fd(fd) };
    for instruction in code {
        file.write_all(&instruction.code.to_ne_bytes())
            .and_then(|()| file.write_all(&[instruction.jt, instruction.jf]))
            .and_then(|()| file.write_all(&instruction.k.to_ne_bytes()))
            .map_err(|error| format!("writing eval seccomp filter: {error}"))?;
    }
    file.rewind()
        .map_err(|error| format!("rewinding eval seccomp filter: {error}"))?;
    // Keep it away from driver fd 3 and stdio. CLOEXEC remains set in the controller.
    // SAFETY: fcntl duplicates the live owned fd with a minimum descriptor number.
    let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) };
    if fd < 0 {
        return Err(format!(
            "duplicating eval seccomp filter: {}",
            io::Error::last_os_error()
        ));
    }
    // SAFETY: the successful duplicate is a fresh independently owned descriptor.
    let file = unsafe { File::from_raw_fd(fd) };
    command.args(["--seccomp", &fd.to_string()]);
    // Bubblewrap reads and closes this descriptor before the interpreter starts. The closure
    // owns the parent copy until Command drops, including failed/cancelled spawn paths.
    // SAFETY: only an async-signal-safe fcntl on a live descriptor runs in the child.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(file.as_raw_fd(), libc::F_SETFD, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

/// A real child verifies installation, native thread creation, and process denial. Cached failures
/// remain failures; neither a missing kernel facility nor an unavailable ABI admits eval.
pub(in crate::tools) fn available() -> Result<(), String> {
    static PROBED: LazyLock<Result<(), String>> = LazyLock::new(probe);
    PROBED.clone()
}

/// Private executable re-entry; does not parse configuration or run user code.
pub const PROBE_ARG: &str = "--dial-private-linux-eval-fence-probe";
const PROBE_OK: &str = "dial-linux-eval-fence-ok-v1";

fn probe() -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let mut command = Command::new(executable);
    command.env_clear();
    #[cfg(not(test))]
    command.arg(PROBE_ARG);
    #[cfg(test)]
    command
        .args([
            "--exact",
            "tools::exec::sandbox::fork_fence::tests::private_probe_entry",
            "--nocapture",
        ])
        .env("DIAL_TEST_FORK_FENCE_PROBE", "1");
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let (status, stdout) = super::run_probe_command(command)?;
    if status != Some(0) || !stdout.lines().any(|line| line == PROBE_OK) {
        return Err(format!(
            "Linux eval process-fork containment probe failed: {status:?} {stdout}"
        ));
    }
    Ok(())
}

/// Runs only in a freshly re-executed private probe process. Never call in the controller.
pub fn probe_main() -> i32 {
    match probe_child() {
        Ok(()) => {
            println!("{PROBE_OK}");
            0
        }
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

fn probe_child() -> Result<(), String> {
    install(&filter()?).map_err(|error| format!("installing Linux eval seccomp: {error}"))?;
    let thread = std::thread::Builder::new()
        .spawn(|| 42)
        .map_err(|error| format!("eval fence blocked runtime threads: {error}"))?;
    if thread.join().map_err(|_| "eval fence thread failed")? != 42 {
        return Err("eval fence thread returned invalid result".into());
    }
    // SAFETY: clone without CLONE_VM acts like fork. On a regression the child immediately
    // exits using only _exit; the parent reaps it below. Arguments contain no dereferenced pointer.
    let pid = unsafe { libc::syscall(libc::SYS_clone, libc::SIGCHLD, 0, 0, 0, 0) };
    if pid == 0 {
        // SAFETY: only the newly created probe child enters this branch; _exit never unwinds.
        unsafe { libc::_exit(92) }
    }
    if pid > 0 {
        // SAFETY: positive PID is this probe's newly created child; waitpid reaps only it.
        unsafe {
            libc::waitpid(pid as i32, std::ptr::null_mut(), 0);
        }
        return Err("eval fence admitted process clone".into());
    }
    if io::Error::last_os_error().raw_os_error() != Some(libc::EPERM) {
        return Err("eval fence did not deny process clone with EPERM".into());
    }
    // SAFETY: invalid pointer with zero size cannot create a child. The fence must return ENOSYS,
    // not EFAULT/EINVAL, so libc's real pthread clone fallback remains available.
    let result = unsafe { libc::syscall(435, 0, 0) };
    if result != -1 || io::Error::last_os_error().raw_os_error() != Some(libc::ENOSYS) {
        return Err("eval fence did not force clone3 fallback".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
