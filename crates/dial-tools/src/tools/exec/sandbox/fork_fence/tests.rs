use super::*;

#[test]
fn private_probe_entry() {
    if std::env::var_os("DIAL_TEST_FORK_FENCE_PROBE").is_some() {
        std::process::exit(probe_main());
    }
}

#[test]
fn real_probe_checks_threads_and_process_denial() {
    available().unwrap();
}

#[test]
fn private_probe_failure_entry() {
    if std::env::var_os("DIAL_TEST_FENCE_FAILURE").is_some() {
        let config = crate::config::types::Config::default();
        let tools = crate::tools::exec::eval::tools(&config);
        assert!(crate::tools::exec::kernel::cached_fork_containment().is_none());
        assert_eq!(
            tools[0].readiness(&config),
            crate::tools::surface::catalog::Readiness::Unknown
        );
        assert!(crate::tools::exec::kernel::cached_fork_containment().is_none());
        // Inherit a stricter outer policy that refuses the inner filter installation.
        let deny_install = [
            stmt(0x20, 0),
            jump(libc::SYS_prctl as u32, 0, 1),
            stmt(0x06, ERRNO | libc::EPERM as u32),
            stmt(0x06, ALLOW),
        ];
        install(&deny_install).unwrap();
        assert!(
            crate::tools::exec::eval::prepare_containment().is_err(),
            "failed kernel installation admitted eval"
        );
        assert!(matches!(
            tools[0].readiness(&config),
            crate::tools::surface::catalog::Readiness::MissingPrerequisite { .. }
        ));
        assert!(crate::tools::surface::catalog::execution_platform("eval").available);
        std::process::exit(0);
    }
}

#[test]
fn real_probe_fails_closed_when_kernel_refuses_installation() {
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tools::exec::sandbox::fork_fence::tests::private_probe_failure_entry",
            "--nocapture",
        ])
        .env("DIAL_TEST_FENCE_FAILURE", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn alternate_abi_syscall_numbers_are_killed_not_admitted() {
    use std::os::unix::process::ExitStatusExt;
    let mut command = Command::new("/usr/bin/python3");
    command.args([
        "-I",
        "-S",
        "-c",
        "import ctypes; ctypes.CDLL(None).syscall(0x40000000)",
    ]);
    direct(&mut command).unwrap();
    assert_eq!(command.status().unwrap().signal(), Some(libc::SIGSYS));
}

const PYTHON_CHECK: &str = r#"
import ctypes, errno, os, subprocess, sys, threading
values = []
t = threading.Thread(target=lambda: values.append(42))
t.start(); t.join()
assert values == [42]
for operation in [lambda: os.fork(),
                  lambda: os.setpgid(0, os.getpgid(os.getppid())),
                  lambda: os.setsid(),
                  lambda: os.posix_spawn(sys.executable, [sys.executable, '-c', 'pass'], os.environ),
                  lambda: subprocess.Popen([sys.executable, '-c', 'pass'], start_new_session=True)]:
    try:
        child = operation()
    except OSError as error:
        assert error.errno == errno.EPERM, error
    else:
        if isinstance(child, int):
            if child == 0: os._exit(94)
            os.waitpid(child, 0)
        else: child.wait()
        raise AssertionError('process creation escaped')
libc = ctypes.CDLL(None, use_errno=True)
assert libc.syscall(435, 0, 0) == -1 and ctypes.get_errno() == errno.ENOSYS
clone = 56 if __import__('platform').machine() == 'x86_64' else 220
for flags in [0x10000, 0x10010900, 0x100010900]:
    assert libc.syscall(clone, ctypes.c_ulonglong(flags), 0, 0, 0, 0) == -1
    assert ctypes.get_errno() == errno.EPERM
assert libc.prctl(39, 0, 0, 0, 0) == 1  # PR_GET_NO_NEW_PRIVS
for fd in os.listdir('/proc/self/fd'):
    try: target = os.readlink('/proc/self/fd/' + fd)
    except FileNotFoundError: continue
    assert 'dial-eval-seccomp' not in target, target
print('contained-python')
"#;
const NODE_CHECK: &str = r#"
const assert = require('node:assert/strict');
const {Worker} = require('node:worker_threads');
const {spawnSync} = require('node:child_process');
(async () => {
  const answer = await new Promise((resolve, reject) => {
    const worker = new Worker('require("node:worker_threads").parentPort.postMessage(42)', {eval:true});
    worker.once('message', resolve); worker.once('error', reject);
  });
  assert.equal(answer, 42);
  for (const detached of [false, true]) {
    const child = spawnSync(process.execPath, ['-e', ''], {detached});
    assert.equal(child.error?.code, 'EPERM');
  }
  console.log('contained-node');
})().catch(error => { console.error(error); process.exitCode = 1; });
"#;

#[test]
fn real_interpreters_keep_threads_but_deny_processes_in_both_launch_modes() {
    for fenced_bubblewrap in [false, true] {
        if fenced_bubblewrap && !super::super::require_for_test() {
            continue;
        }
        for (program, flags, code, expected) in [
            (
                "/usr/bin/python3",
                vec!["-I", "-S", "-c"],
                PYTHON_CHECK,
                "contained-python",
            ),
            (
                "/usr/local/bin/node",
                vec!["--no-addons", "-e"],
                NODE_CHECK,
                "contained-node",
            ),
        ] {
            // Node is optional in developer environments; an explicit path can select an installed
            // Node 22+ without fetching dependencies or making the default suite network-dependent.
            let node = std::env::var("DIAL_TEST_NODE").unwrap_or_else(|_| program.into());
            let program = if expected == "contained-node" {
                node.as_str()
            } else {
                program
            };
            if !std::path::Path::new(program).is_file() {
                assert!(
                    std::env::var_os("DIAL_REQUIRE_EVAL").is_none(),
                    "missing {program}"
                );
                eprintln!("skipping optional interpreter {program}");
                continue;
            }
            let mut command;
            if fenced_bubblewrap {
                command = Command::new("/usr/bin/bwrap");
                command.args([
                    "--unshare-user",
                    "--unshare-pid",
                    "--unshare-net",
                    "--cap-drop",
                    "ALL",
                    "--ro-bind",
                    "/",
                    "/",
                    "--proc",
                    "/proc",
                    "--dev",
                    "/dev",
                ]);
                bubblewrap(&mut command).unwrap();
                command.arg("--").arg(program);
            } else {
                command = Command::new(program);
                direct(&mut command).unwrap();
            }
            let output = command.env_clear().args(flags).arg(code).output().unwrap();
            assert!(
                output.status.success(),
                "bwrap={fenced_bubblewrap}, program={program}: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains(expected));
        }
    }
}
