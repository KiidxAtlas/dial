use super::*;

#[test]
fn proc_net_addresses_decode_host_order_words() {
    assert_eq!(
        parse_net_address("0100007F:1F90"),
        Some(("127.0.0.1".into(), 8080))
    );
    assert_eq!(
        parse_net_address("00000000:0016"),
        Some(("0.0.0.0".into(), 22))
    );
    assert_eq!(
        parse_net_address("00000000000000000000000001000000:0050"),
        Some(("::1".into(), 80))
    );
    assert_eq!(parse_net_address("0100007:1F90"), None);
    assert_eq!(parse_net_address("zz00007F:1F90"), None);
}

#[test]
fn proc_net_tables_keep_listening_tcp_and_bound_udp_only() {
    let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 4242 1 0\n\
   1: 0100007F:9C40 0100007F:1F90 01 00000000:00000000 00:00000000 00000000  1000        0 4243 1 0\n";
    let rows = parse_net_table("tcp", tcp);
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].0.port, rows[0].1), (8080, 4242));
    let udp = "header\n   5: 00000000:0035 00000000:0000 07 00000000:00000000 00:00000000 00000000 0 0 77 2 0\n";
    assert_eq!(parse_net_table("udp", udp)[0].0.port, 53);
    assert!(parse_net_table("tcp", udp).is_empty());
}

#[test]
fn proc_stat_survives_spaces_and_parentheses_in_the_name() {
    let mut stat = String::from("42 (my (odd) name) S 7");
    for field in 5..=24 {
        stat.push_str(&format!(" {field}"));
    }
    let (comm, state, ppid, start, rss) = parse_stat(&stat).unwrap();
    assert_eq!(
        (comm.as_str(), state.as_str(), ppid),
        ("my (odd) name", "S", 7)
    );
    // stat(5): starttime is field 22 and rss is field 24.
    assert_eq!((start, rss), (22, 24));
}

#[test]
fn ps_rows_and_elapsed_times_parse() {
    let rows = parse_ps(
        "    1     0 Ss    1024      01:02 /sbin/launchd\n  77    1 R+  2048  1-02:03:04 python3 -m http.server 8000\n",
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].elapsed_secs, Some(62));
    assert_eq!(rows[1].elapsed_secs, Some(86400 + 7384));
    assert_eq!(rows[1].rss_bytes, Some(2048 * 1024));
    assert_eq!(rows[1].state, "R");
    assert_eq!(rows[1].command, "python3 -m http.server 8000");
    assert_eq!(parse_etime("bad"), None);
}

#[test]
fn lsof_records_yield_listening_endpoints_with_owners() {
    let text = "p10\ncnginx\nf6\nPTCP\nn*:80\nf7\nPTCP\nn[::1]:8443\np11\ncdnsd\nf3\nPUDP\nn127.0.0.1:53\nf4\nPUDP\nn10.0.0.2:5000->10.0.0.3:6000\n";
    let sockets = parse_lsof_sockets(text);
    assert_eq!(sockets.len(), 3);
    assert_eq!(
        (
            sockets[0].protocol,
            sockets[0].address.as_str(),
            sockets[0].port,
            sockets[0].pid
        ),
        ("tcp", "0.0.0.0", 80, Some(10))
    );
    assert_eq!(
        (sockets[1].protocol, sockets[1].address.as_str()),
        ("tcp6", "::1")
    );
    assert_eq!(
        (sockets[2].protocol, sockets[2].program.as_deref()),
        ("udp", Some("dnsd"))
    );
}

#[test]
fn rendered_commands_are_credential_redacted() {
    let secret = "ghp_".to_owned() + &"a".repeat(36);
    let out = render_procs(
        vec![Proc {
            pid: 9,
            ppid: 1,
            state: "S".into(),
            rss_bytes: Some(3 << 20),
            elapsed_secs: Some(3700),
            command: format!("server --token {secret}"),
        }],
        "test",
        0,
    );
    assert!(!out.contains(&secret), "{out}");
    assert!(out.contains("3.0M") && out.contains("1h01m"), "{out}");
}

#[test]
fn unknown_views_list_the_supported_ones() {
    let cancel = AtomicBool::new(false);
    let error = read("sys://nope", &cancel).unwrap_err();
    assert!(
        error.contains("sys://procs") && error.contains("sys://ports"),
        "{error}"
    );
    assert!(read("sys://", &cancel).unwrap().contains("sys://host"));
}

#[test]
fn live_views_show_this_process_and_its_listening_socket() {
    use crate::tools::testkit::*;
    use serde_json::json;
    let (_dir, ws) = workspace();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let own = std::process::id().to_string();

    let procs = run(&ws, "read", json!({"path": "sys://procs"}));
    assert!(!procs.is_error, "{}", procs.text);
    assert!(procs.text.contains("[dial]"), "{}", procs.text);

    let ports = run(&ws, "read", json!({"path": "sys://ports"}));
    assert!(!ports.is_error, "{}", ports.text);
    let row = ports
        .text
        .lines()
        .find(|line| line.contains(&format!(" {port} ")))
        .unwrap_or_else(|| panic!("port {port} missing:\n{}", ports.text));
    assert!(row.contains(&own), "{row}");

    let detail = run(&ws, "read", json!({"path": format!("sys://procs/{own}")}));
    assert!(!detail.is_error, "{}", detail.text);
    assert!(detail.text.contains(&format!(":{port}")), "{}", detail.text);
    if cfg!(windows) {
        assert!(detail.text.contains("threads: "), "{}", detail.text);
        assert!(
            detail
                .text
                .contains("open files: not listed on this platform"),
            "{}",
            detail.text
        );
    }

    assert!(run(&ws, "read", json!({"path": "sys://procs", "limit": 5})).is_error);
}

#[test]
fn inventory_reports_this_machine_and_renders_compactly() {
    let dir = tempfile::tempdir().unwrap();
    let found = inventory(Some(dir.path()));
    assert_eq!(found.arch, std::env::consts::ARCH);
    assert!(found.cpus >= 1);
    let compact = found.compact();
    assert!(compact.starts_with("- Machine: "), "{compact}");
    assert!(found.render().contains("toolchains:"));
}

#[test]
fn compact_inventory_shortens_versions_and_names_missing_tools() {
    let found = Inventory {
        os: "Debian 12".into(),
        arch: "aarch64".into(),
        cpus: 4,
        memory_total: Some(8 << 30),
        toolchains: vec![
            ("python3".into(), Some("Python 3.12.1".into())),
            ("node".into(), Some("v22.3.0".into())),
            ("java".into(), None),
        ],
        present: vec!["curl".into()],
        missing: vec!["gcc".into()],
        root: true,
        ..Inventory::default()
    };
    let live = "- Live host state: read `sys://procs`, `sys://ports`, `sys://host`.\n";
    assert_eq!(
        found.compact(),
        format!(
            "- Machine: Debian 12 aarch64, 4 CPUs, 8.0G RAM, running as root.\n\
             - On PATH: python3 3.12.1, node 22.3.0, java, curl.\n\
             - Not installed: gcc.\n{live}"
        )
    );
}

/// Lookup without running anything: a non-executable file is skipped, and on Windows the
/// `PATHEXT` suffix is supplied (`tool` finds `tool.exe`). `PATHEXT` is conventionally upper
/// case (`.EXE`) and Windows paths are case-insensitive, so the match ignores case there.
#[test]
fn path_lookup_finds_executables_only() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("plain");
    let empty = tempfile::tempdir().unwrap();
    std::fs::create_dir(&plain).unwrap();
    let name = if cfg!(windows) { "tool.exe" } else { "tool" };
    std::fs::write(plain.join(name), "").unwrap();
    std::fs::write(plain.join("data"), "").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(plain.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = std::env::join_paths([empty.path(), plain.as_path()]).unwrap();
    let found = find_on_path("tool", &path).expect("the executable is found");
    if cfg!(windows) {
        assert!(
            found.as_os_str().eq_ignore_ascii_case(plain.join(name)),
            "{found:?}"
        );
    } else {
        assert_eq!(found, plain.join(name));
    }
    assert_eq!(find_on_path("data", &path), None);
    assert_eq!(find_on_path("absent", &path), None);
}

/// A tool whose version probe exits unsuccessfully (a broken shim, or the Windows Store
/// `python3` alias) is neither listed nor treated as installed.
#[cfg(unix)]
#[test]
fn failing_version_probes_count_as_missing() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let shim = dir.path().join("probe-shim");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    for (script, expected) in [
        (
            "#!/bin/sh\necho 'not really installed' >&2\nexit 9\n",
            inventory::Probe::Failed,
        ),
        (
            "#!/bin/sh\necho 'shim 1.2.3'\n",
            inventory::Probe::Ran(Some("shim 1.2.3".into())),
        ),
    ] {
        std::fs::write(&shim, script).unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(inventory::version(&shim, &[], deadline), expected);
    }
}
