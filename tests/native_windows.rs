//! Native Windows contracts: real pipes, process trees, protected storage and Git Bash.
#![cfg(windows)]
use dial::{
    platform,
    tools::{self, Workspace},
};
use serde_json::json;
use std::{
    fs,
    io::{Read, Write},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[test]
fn private_files_reject_hardlinks_and_preserve_append() {
    let dir = tempfile::tempdir().unwrap();
    platform::fs::private(dir.path()).unwrap();
    let path = dir.path().join("private 雪.txt");
    let mut options = platform::fs::OpenOptions::new();
    options
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(platform::fs::NOFOLLOW);
    options.open(&path).unwrap().write_all(b"one").unwrap();
    let mut append = platform::fs::OpenOptions::new();
    append
        .append(true)
        .mode(0o600)
        .custom_flags(platform::fs::NOFOLLOW);
    append.open(&path).unwrap().write_all(b"two").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"onetwo");
    let file = fs::File::open(&path).unwrap();
    assert!(platform::fs::owned(&file).unwrap());
    assert!(platform::fs::single_link(&file).unwrap());
    fs::hard_link(&path, dir.path().join("alias")).unwrap();
    assert!(append.open(&path).is_err());
}

#[test]
fn git_bash_runs_in_unicode_workspace_and_times_out() {
    let dir = tempfile::Builder::new()
        .prefix("dial space 雪 ")
        .tempdir()
        .unwrap();
    let ws = Workspace::new(dir.path()).unwrap().without_sandbox();
    let output = tools::execute(
        &ws,
        "bash",
        &json!({"command":"printf 'hello'; printf 'error' >&2; printf 'saved' > file.txt"}),
        10,
    );
    assert!(!output.is_error, "{}", output.text);
    assert!(output.text.contains("hello") && output.text.contains("error"));
    assert_eq!(
        fs::read_to_string(dir.path().join("file.txt")).unwrap(),
        "saved"
    );
    let start = Instant::now();
    let output = tools::execute(&ws, "bash", &json!({"command":"sleep 30"}), 1);
    assert!(output.is_error, "{}", output.text);
    assert!(start.elapsed() < Duration::from_secs(5));
    let confined = Workspace::new(dir.path()).unwrap();
    let output = tools::execute(&confined, "bash", &json!({"command":"touch escaped"}), 1);
    assert!(output.is_error && !dir.path().join("escaped").exists());
}

fn python(code: &str) -> Command {
    let mut command = Command::new("python");
    command
        .args(["-u", "-c", code])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}
#[test]
fn cancellation_terminates_native_descendants() {
    let mut child = platform::process::spawn(python("import subprocess,sys,time; p=subprocess.Popen([sys.executable,'-c','import time;time.sleep(60)'],creationflags=8);print(p.pid,flush=True);time.sleep(60)")).unwrap();
    let mut reader = platform::pipe::Reader::new(child.stdout.take().unwrap()).unwrap();
    let mut bytes = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !bytes.contains(&b'\n') {
        let mut chunk = [0; 64];
        match reader.read(&mut chunk) {
            Ok(n) if n > 0 => bytes.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10))
            }
            result => panic!("read: {result:?}"),
        }
        assert!(Instant::now() < deadline, "child did not start");
    }
    let pid: i32 = String::from_utf8(bytes).unwrap().trim().parse().unwrap();
    assert!(platform::process::pid_alive(pid));
    platform::process::terminate(&mut child);
    child.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while platform::process::pid_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !platform::process::pid_alive(pid),
        "detached descendant survived cancellation"
    );
}
#[test]
fn stalled_stdin_does_not_block_deadline() {
    let mut child = platform::process::spawn(python("import time;time.sleep(60)")).unwrap();
    let mut writer = platform::pipe::Writer::new(child.stdin.take().unwrap()).unwrap();
    let data = vec![b'x'; 1024 * 1024];
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        match writer.write(&data) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => panic!("{e}"),
        }
    }
    assert!(Instant::now() - deadline < Duration::from_secs(1));
    platform::process::terminate(&mut child);
    child.wait().unwrap();
}
#[test]
fn paths_and_programs_are_native() {
    assert_eq!(
        platform::shell_path(std::path::Path::new(r"\\?\C:\Users\Space 雪\file.txt")),
        "C:/Users/Space 雪/file.txt"
    );
    let dirs = std::env::split_paths(&std::env::var_os("PATH").unwrap()).collect::<Vec<_>>();
    assert!(platform::programs::find(std::path::Path::new("python"), &dirs).is_some());
    assert!(platform::programs::bash().unwrap().is_file());
}

#[test]
fn background_and_python_kernel_work_natively() {
    use dial::{
        config::types::Config,
        tools::{background, ext::ToolContext, kernel},
    };
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap().without_sandbox();
    let config = Config::default();
    let ctx = ToolContext {
        workspace: &ws,
        config: &config,
    };
    let background = background::tools(&config).remove(0);
    let started = background.call(
        &ctx,
        &json!({"action":"start", "name":"native", "command":"printf 'ready'; sleep 30"}),
    );
    assert!(!started.is_error, "{}", started.text);
    let ready = background.call(
        &ctx,
        &json!({"action":"wait", "name":"native", "until":"ready", "timeout":5}),
    );
    assert!(
        !ready.is_error && ready.text.contains("ready"),
        "{}",
        ready.text
    );
    let killed = background.call(&ctx, &json!({"action":"kill", "name":"native"}));
    assert!(!killed.is_error, "{}", killed.text);
    let kernel = kernel::tools(&config).remove(0);
    let first = kernel.call(&ctx, &json!({"action":"eval", "code":"x = 40\nx + 2"}));
    assert!(
        !first.is_error && first.text.contains("42"),
        "{}",
        first.text
    );
    let retained = kernel.call(&ctx, &json!({"action":"eval", "code":"x + 3"}));
    assert!(
        !retained.is_error && retained.text.contains("43"),
        "{}",
        retained.text
    );
    let timed_out = kernel.call(
        &ctx,
        &json!({"action":"eval", "code":"import time; time.sleep(30)", "timeout":1}),
    );
    assert!(timed_out.is_error, "{}", timed_out.text);
    let fresh = kernel.call(&ctx, &json!({"action":"eval", "code":"'x' in globals()"}));
    assert!(
        !fresh.is_error && fresh.text.contains("False"),
        "{}",
        fresh.text
    );
}

#[test]
fn native_file_paths_and_state_survive_reopen() {
    use dial::storage::{
        paths::Layout,
        settings_store::{self},
        store::Store,
    };
    let project = tempfile::Builder::new()
        .prefix("dial project 雪 ")
        .tempdir()
        .unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().join("state")).unwrap();
    settings_store::save_history(&layout, &["hello 雪".into()], 10).unwrap();
    assert_eq!(
        settings_store::load_history(&layout, 10).unwrap(),
        ["hello 雪"]
    );
    let store = Store::open(&layout.state_dir, "model", "coding", &Default::default()).unwrap();
    drop(store);
    Store::open(&layout.state_dir, "model", "coding", &Default::default()).unwrap();
    let ws = Workspace::new(project.path()).unwrap();
    fs::write(project.path().join("file.txt"), "hello").unwrap();
    assert_eq!(
        ws.resolve(project.path().join("file.txt").to_str().unwrap())
            .unwrap(),
        project.path().join("file.txt").canonicalize().unwrap()
    );
    assert!(ws.resolve("file.txt:hidden").is_err());
}
