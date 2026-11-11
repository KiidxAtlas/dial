use super::*;
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
