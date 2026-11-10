use super::*;
use std::process::{Command, Stdio};

fn sleeper(env: &[(&str, &str)]) -> std::process::Child {
    let mut command = Command::new("ping");
    command
        .args(["-n", "30", "127.0.0.1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (name, value) in env {
        command.env(name, value);
    }
    command.spawn().expect("ping is part of Windows")
}

#[test]
fn snapshot_and_details_describe_this_process() {
    let own = std::process::id();
    let entry = snapshot()
        .unwrap()
        .into_iter()
        .find(|entry| entry.pid == own)
        .expect("the snapshot lists this process");
    assert!(entry.threads >= 1);
    assert!(
        entry.image.to_ascii_lowercase().ends_with(".exe"),
        "{entry:?}"
    );
    let details = details(own).expect("this process can open itself");
    assert!(!details.exited);
    assert!(details.working_set.is_some_and(|bytes| bytes > 0));
    assert!(age_secs(details.created).is_some());
    let exe = std::env::current_exe().unwrap();
    let name = exe
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_ascii_lowercase();
    assert!(
        details
            .command_line
            .as_deref()
            .is_some_and(|line| line.to_ascii_lowercase().contains(&name)),
        "{details:?}"
    );
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(
        details.cwd.as_deref().map(str::to_ascii_lowercase),
        Some(cwd.to_string_lossy().to_ascii_lowercase()),
    );
    assert!(
        details
            .executable
            .is_some_and(|path| path.to_ascii_lowercase().ends_with(&name))
    );
}

#[test]
fn kill_marked_requires_the_exact_environment_entry_and_creation_window() {
    let token = format!("{:x}", std::process::id() as u64 * 7919 + 17);
    let started = filetime_now().saturating_sub(10_000_000);
    let mut child = sleeper(&[("DIAL_PROCESSES_TEST", &token)]);
    let pid = child.id();
    assert!(!kill_marked(pid, "DIAL_PROCESSES_TEST=other", started).unwrap());
    assert!(!kill_marked(pid, &format!("DIAL_PROCESSES_TEST={token}x"), started).unwrap());
    assert!(
        !kill_marked(
            pid,
            &format!("DIAL_PROCESSES_TEST={token}"),
            filetime_now() + 10_000_000
        )
        .unwrap(),
        "a process created before the window is never marked"
    );
    assert!(
        child.try_wait().unwrap().is_none(),
        "unmarked checks leave it running"
    );
    assert!(kill_marked(pid, &format!("DIAL_PROCESSES_TEST={token}"), started).unwrap());
    let status = child.wait().unwrap();
    assert!(!status.success());
    assert!(!kill_marked(pid, &format!("DIAL_PROCESSES_TEST={token}"), started).unwrap());
}

#[test]
fn listeners_name_this_process_as_owner() {
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let own = std::process::id();
    let found = listeners().unwrap();
    let has = |protocol: &str, port: u16| {
        found.iter().any(|listener| {
            listener.protocol == protocol
                && listener.port == port
                && listener.pid == own
                && listener.address == IpAddr::V4(Ipv4Addr::LOCALHOST)
        })
    };
    assert!(has("tcp", tcp.local_addr().unwrap().port()), "{found:?}");
    assert!(has("udp", udp.local_addr().unwrap().port()), "{found:?}");
}

#[test]
fn table_rows_stop_at_the_buffer_end() {
    let mut bytes = 3u32.to_ne_bytes().to_vec();
    for value in [1u32, 2, 3, 4, 5, 6] {
        bytes.extend(value.to_ne_bytes());
    }
    let parsed: Vec<MIB_UDPROW_OWNER_PID> = rows(&bytes);
    assert_eq!(parsed.len(), 2, "the third row is truncated");
    assert_eq!(parsed[1].dwLocalAddr, 4);
    assert_eq!(port(u32::from(8080u16.to_be())), 8080);
}
