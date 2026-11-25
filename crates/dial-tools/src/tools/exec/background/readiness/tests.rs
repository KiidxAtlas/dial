use super::*;
use serde_json::json;
use std::net::{Ipv4Addr, Ipv6Addr};

const GROUP_PID: u32 = 40;

fn owned(pid: u32) -> bool {
    pid == GROUP_PID
}

fn v4(port: u16) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, port))
}

#[test]
fn an_owned_tcp_listener_on_the_port_is_ready() {
    let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
    assert!(owned_listener(
        [("tcp", loopback, 3000, GROUP_PID)],
        v4(3000),
        owned
    ));
    // Wildcard sockets of either family count, as lsof's `*:port` does.
    for wildcard in [
        IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        IpAddr::V6(Ipv6Addr::UNSPECIFIED),
    ] {
        let protocol = if wildcard.is_ipv4() { "tcp" } else { "tcp6" };
        assert!(owned_listener(
            [(protocol, wildcard, 3000, GROUP_PID)],
            v4(3000),
            owned
        ));
    }
}

#[test]
fn foreign_wrong_port_udp_and_other_addresses_are_not_ready() {
    let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
    // Another process listening on the port proves nothing about this job.
    assert!(!owned_listener(
        [("tcp", loopback, 3000, 7)],
        v4(3000),
        owned
    ));
    assert!(!owned_listener(
        [("tcp", loopback, 3001, GROUP_PID)],
        v4(3000),
        owned
    ));
    assert!(!owned_listener(
        [("udp", loopback, 3000, GROUP_PID)],
        v4(3000),
        owned
    ));
    let other = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));
    assert!(!owned_listener(
        [("tcp", other, 3000, GROUP_PID)],
        v4(3000),
        owned
    ));
    // A specific IPv6 socket does not answer an IPv4 address.
    let v6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
    assert!(!owned_listener(
        [("tcp6", v6, 3000, GROUP_PID)],
        v4(3000),
        owned
    ));
    assert!(owned_listener(
        [("tcp6", v6, 3000, GROUP_PID)],
        SocketAddr::from((Ipv6Addr::LOCALHOST, 3000)),
        owned
    ));
}

#[test]
fn ready_without_a_log_or_port_names_both_probes_and_an_example() {
    // A timeout bounds a probe; on its own it has nothing to wait for.
    for ready in [json!({"timeout": 30}), json!({})] {
        let error = Ready::parse(&json!({"ready": ready})).err().unwrap();
        assert!(
            error.contains("ready.log")
                && error.contains("ready.port")
                && error.contains("\"log\""),
            "{error}"
        );
    }
    assert!(Ready::parse(&json!({"ready": {"log": "listening", "timeout": 30}})).is_ok());
}

#[test]
fn proc_net_tables_yield_only_listening_sockets_with_decoded_addresses() {
    // Header, 127.0.0.1:5328 LISTEN, 0.0.0.0:8080 LISTEN, an ESTABLISHED socket.
    let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 0100007F:14D0 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 4242 1 0 100 0 0 10 0\n\
   1: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 4343 1 0 100 0 0 10 0\n\
   2: 0100007F:14D0 0100007F:C350 01 00000000:00000000 00:00000000 00000000     0        0 4444 1 0 20 4 30 10 -1\n";
    assert_eq!(
        proc_net_listeners(tcp),
        vec![
            (IpAddr::V4(Ipv4Addr::LOCALHOST), 5328, 4242),
            (IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8080, 4343),
        ]
    );
    // ::1 is printed as four little-endian words.
    let tcp6 = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 00000000000000000000000001000000:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 5151 1 0 100 0 0 10 0\n";
    assert_eq!(
        proc_net_listeners(tcp6),
        vec![(IpAddr::V6(Ipv6Addr::LOCALHOST), 8080, 5151)]
    );
}

#[cfg(target_os = "linux")]
#[test]
fn procfs_proves_a_listener_belongs_to_the_group_without_lsof() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    // SAFETY: getpgid(0) queries this process's own group and borrows no memory.
    let own_group = unsafe { libc::getpgid(0) } as u32;
    assert!(procfs_owned_listener(own_group, address).unwrap());
    assert!(!procfs_owned_listener(own_group.wrapping_add(1_000_000), address).unwrap());
    drop(listener);
    assert!(!procfs_owned_listener(own_group, address).unwrap());
}
