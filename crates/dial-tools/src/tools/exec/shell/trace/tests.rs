use super::*;

const TRACE: &str = r#"100   execve("/usr/bin/bash", ["/usr/bin/bash", "-c", "python3 app.py"], 0x7ffd /* 20 vars */) = 0
100   openat(AT_FDCWD, "/etc/ld.so.cache", O_RDONLY|O_CLOEXEC) = 3
100   execve("/usr/local/sbin/python3", ["python3", "app.py"], 0x55 /* 20 vars */) = -1 ENOENT (No such file or directory)
100   execve("/usr/bin/python3", ["python3", "app.py"], 0x55 /* 20 vars */) = 0
100   openat(AT_FDCWD, "/usr/lib/python3.11/encodings/__pycache__/x.pyc", O_RDONLY) = -1 ENOENT (No such file or directory)
100   newfstatat(AT_FDCWD, "/app/config.yaml", 0x7ffc, 0) = -1 ENOENT (No such file or directory)
100   openat(AT_FDCWD, "/app/data.csv", O_RDONLY|O_CLOEXEC) = 3
100   openat(AT_FDCWD, "/app/out/report.txt", O_WRONLY|O_CREAT|O_TRUNC|O_CLOEXEC, 0666) = 4
100   openat(AT_FDCWD, "/var/log/app.log", O_WRONLY|O_APPEND <unfinished ...>
101   execve("/usr/bin/gcc", ["gcc", "-O2", "x.c"], 0x55 /* 20 vars */) = 0
100   <... openat resumed>) = -1 EACCES (Permission denied)
100   connect(5, {sa_family=AF_INET, sin_port=htons(5432), sin_addr=inet_addr("127.0.0.1")}, 16) = -1 ECONNREFUSED (Connection refused)
100   bind(6, {sa_family=AF_INET6, sin6_port=htons(8080), sin6_flowinfo=htonl(0), inet_pton(AF_INET6, "::", &sin6_addr), sin6_scope_id=0}, 28) = 0
100   connect(7, {sa_family=AF_UNIX, sun_path="/var/run/docker.sock"}, 110) = -1 ENOENT (No such file or directory)
101   --- SIGSEGV {si_signo=SIGSEGV, si_code=SEGV_MAPERR, si_addr=NULL} ---
101   +++ killed by SIGSEGV (core dumped) +++
102   execve("/opt/tool/bin/missing-helper", ["missing-helper"], 0x55 /* 1 var */) = -1 ENOENT (No such file or directory)
100   +++ exited with 1 +++
"#;

#[test]
fn summary_reports_behavior_and_filters_search_probes() {
    let out = summarize(TRACE);
    let has = |needle: &str| assert!(out.contains(needle), "missing {needle:?} in:\n{out}");
    has("executed (3):");
    has("python3 app.py");
    has("gcc -O2 x.c");
    has("pid 101 (gcc) killed by SIGSEGV (core dumped)");
    has("pid 100 (python3) exited with 1");
    has("/app/config.yaml (newfstatat)");
    has("/opt/tool/bin/missing-helper (execve ENOENT)");
    has("/var/log/app.log (openat EACCES)");
    has("connect 127.0.0.1:5432 ECONNREFUSED (python3)");
    has("bind [::]:8080 ok (python3)");
    has("connect unix:/var/run/docker.sock ENOENT (python3)");
    has("/app/out/report.txt (openat)");
    has("read (1):\n  /app/data.csv");
    has("[1 library/module search probes that found nothing are omitted]");
    // A PATH lookup that later succeeded is not a missing program.
    assert!(!out.contains("/usr/local/sbin/python3"), "{out}");
    assert!(!out.contains("ld.so.cache"), "{out}");
}

#[test]
fn listings_are_bounded_and_point_at_the_raw_trace() {
    let mut trace = String::new();
    for i in 0..40 {
        trace.push_str(&format!(
            "7 openat(AT_FDCWD, \"/srv/f{i}\", O_RDONLY) = -1 ENOENT (No such file or directory)\n"
        ));
    }
    let out = summarize(&trace);
    assert!(out.contains("not found (40):"), "{out}");
    assert!(out.contains("[15 more in the raw trace]"), "{out}");
    assert!(out.starts_with("[trace: strace -f summary; raw trace .tmp/t.txt]"));
}

#[test]
fn quoting_survives_single_quotes() {
    assert_eq!(quote("echo 'hi'"), r"'echo '\''hi'\'''");
}

#[test]
fn traced_requests_refuse_background_modes_and_unsupported_hosts() {
    let (_dir, ws) = crate::tools::testkit::workspace();
    let args = |extra: serde_json::Value| {
        let mut args = serde_json::json!({"command": "true", "trace": true});
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        args
    };
    let error = prepare(&ws, &args(serde_json::json!({"background": true})), "true")
        .err()
        .unwrap();
    assert!(error.contains("background"), "{error}");
    assert!(
        prepare(&ws, &serde_json::json!({"command": "true"}), "true")
            .unwrap()
            .is_none()
    );
    if !cfg!(target_os = "linux") {
        let error = prepare(&ws, &args(serde_json::json!({})), "true")
            .err()
            .unwrap();
        assert!(error.contains("nothing was run"), "{error}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn live_trace_summarizes_a_real_command_when_strace_is_installed() {
    let path = std::env::var_os("PATH").unwrap_or_default();
    if crate::tools::fs::system::find_on_path("strace", &path).is_none() {
        eprintln!("strace not installed; live trace test skipped");
        return;
    }
    let (_dir, ws) = crate::tools::testkit::workspace();
    let out = crate::tools::testkit::run(
        &ws,
        "bash",
        serde_json::json!({"command": "cat /nonexistent-dial-trace-probe; echo done > made.txt", "trace": true}),
    );
    assert!(
        out.text.contains("[trace: strace -f summary"),
        "{}",
        out.text
    );
    assert!(
        out.text.contains("/nonexistent-dial-trace-probe"),
        "{}",
        out.text
    );
    assert!(out.text.contains("made.txt"), "{}", out.text);
}
