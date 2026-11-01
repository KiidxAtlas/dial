use super::*;

/// This test doubles as a subprocess entry point, running the real relay bind/startup path.
#[test]
fn relay_readiness_child() {
    let Some(bind) = std::env::var_os("DIAL_TEST_RELAY_BIND") else {
        return;
    };
    if let Err(error) = crate::collab::server::serve(bind.to_string_lossy().parse().unwrap()) {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

#[test]
fn relay_readiness_requires_successful_bind_in_the_launched_child() {
    use std::process::{Child, Command, Stdio};
    struct TestChild(Child);
    impl Drop for TestChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    // Reserving a real listener proves a bind failure cannot be mistaken for readiness from
    // some other process already listening there. Port zero selects a free bind for success.
    let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    for (bind, ready) in [
        ("127.0.0.1:0".to_owned(), true),
        (reserved.local_addr().unwrap().to_string(), false),
    ] {
        let mut child = TestChild(
            crate::platform::process::spawn(
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "app::commands::maintenance::tests::relay_readiness_child",
                        "--nocapture",
                    ])
                    .env("DIAL_TEST_RELAY_BIND", bind)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped()),
            )
            .unwrap(),
        );
        let result = await_relay_ready(&mut child.0);
        assert_eq!(result.is_ok(), ready, "{result:?}");
        if !ready {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("binding collab relay")
            );
        }
    }
}
