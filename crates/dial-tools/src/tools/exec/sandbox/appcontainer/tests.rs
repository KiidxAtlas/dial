use super::*;

fn args(items: &[&str]) -> impl Iterator<Item = OsString> {
    items
        .iter()
        .map(OsString::from)
        .collect::<Vec<_>>()
        .into_iter()
}

#[test]
fn strict_and_offline_policies_keep_their_paths_and_network_setting() {
    let deny = vec![PathBuf::from("/home/u/.ssh")];
    let extra = vec![PathBuf::from("/home/u/.cargo")];
    let strict = Policy::Strict {
        deny_read: deny.clone(),
        allow_network: true,
        extra_write: extra.clone(),
    };
    assert_eq!(
        plan(&strict),
        Plan {
            deny_read: &deny,
            extra_write: &extra,
            network: true
        }
    );
    let offline = Policy::Dev {
        deny_read: deny.clone(),
        extra_write: extra.clone(),
        allow_network: false,
    };
    assert!(!plan(&offline).network);
    assert!(capabilities(false).is_empty());
    assert_eq!(capabilities(true), ["S-1-15-3-1", "S-1-15-3-3"]);
}

#[test]
fn profile_names_are_stable_short_and_per_workspace() {
    let a = profile_name(Path::new(r"C:\Users\u\Project"));
    assert_eq!(a, profile_name(Path::new(r"c:\users\u\project")));
    assert_ne!(a, profile_name(Path::new(r"C:\Users\u\Other")));
    assert!(a.starts_with(PROFILE_PREFIX) && a.len() <= 64, "{a}");
    assert!(parse(args(&[&a, "pwsh"])).is_ok());
}

#[test]
fn launcher_arguments_parse_in_order_and_refuse_foreign_profiles() {
    let launch = parse(args(&[
        "dial.sandbox.00ff",
        "--dial-network",
        "--dial-child-fence",
        "node",
        "--dial-network",
        "-e",
    ]))
    .unwrap();
    assert_eq!(
        launch,
        Launch {
            profile: "dial.sandbox.00ff".into(),
            network: true,
            fence: true,
            program: "node".into(),
            args: vec!["--dial-network".into(), "-e".into()],
        }
    );
    let plain = parse(args(&["dial.sandbox.ab", "cmd"])).unwrap();
    assert!(!plain.network && !plain.fence && plain.args.is_empty());
    for bad in [
        &[][..],
        &["other.app", "cmd"],
        &["dial.sandbox.", "cmd"],
        &["dial.sandbox.zz", "cmd"],
        &["dial.sandbox.ab"],
        &["dial.sandbox.ab", "--dial-network"],
    ] {
        assert!(parse(args(bad)).is_err(), "{bad:?}");
    }
}
