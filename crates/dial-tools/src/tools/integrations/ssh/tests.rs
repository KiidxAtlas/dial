#![cfg(unix)]

use super::*;
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn staged_write_ignores_predictable_symlink_and_preserves_destination() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let dir = tempfile::tempdir().expect("temporary directory");
    let victim = dir.path().join("victim");
    std::fs::write(&victim, "untouched").expect("victim fixture");
    for existing in [false, true] {
        let dest = dir.path().join(if existing { "existing" } else { "new" });
        let target = dir.path().join("target");
        if existing {
            std::fs::write(&target, "old").expect("target fixture");
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640))
                .expect("target mode");
            symlink(&target, &dest).expect("destination link");
        }
        let path = url::sh_quote(&dest.to_string_lossy());
        // The attack is planted by the shell itself so $$ matches the staging process exactly.
        let script = format!(
            "dest={path}; ln -s {} \"${{dest}}.dial-tmp-$$\"; {}",
            url::sh_quote(&victim.to_string_lossy()),
            write_script(&path)
        );
        let mut child = Command::new("sh")
            .args(["-c", &script])
            .stdin(Stdio::piped())
            .spawn()
            .expect("local POSIX shell fixture");
        child
            .stdin
            .take()
            .expect("piped input")
            .write_all(b"new bytes")
            .expect("write transfer input");
        assert!(
            child.wait().expect("shell completion").success(),
            "existing={existing}"
        );
        assert_eq!(
            std::fs::read_to_string(&victim).expect("victim bytes"),
            "untouched"
        );
        assert_eq!(
            std::fs::read_to_string(&dest).expect("destination bytes"),
            "new bytes"
        );
        if existing {
            assert!(dest.is_symlink(), "existing destination link retained");
            assert_eq!(
                std::fs::metadata(&dest)
                    .expect("target metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o640
            );
        }
        assert!(
            std::fs::read_dir(dir.path())
                .expect("staging directory")
                .all(|entry| !entry
                    .expect("directory entry")
                    .file_name()
                    .to_string_lossy()
                    .contains(".dial-stage-")),
            "staging cleaned"
        );
    }
}

#[test]
fn failed_staging_leaves_destination_untouched_and_cleans_private_directory() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("temporary directory");
    let dest = dir.path().join("destination");
    std::fs::write(&dest, "old bytes").expect("destination fixture");
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).expect("fixture binaries");
    let cat = bin.join("cat");
    for interrupted in [false, true] {
        let signal = if interrupted {
            "kill -TERM \"$STAGING_SHELL\";"
        } else {
            ""
        };
        std::fs::write(
            &cat,
            format!("#!/bin/sh\nprintf partial; {signal} exit 1\n"),
        )
        .expect("failing transfer fixture");
        std::fs::set_permissions(&cat, std::fs::Permissions::from_mode(0o700))
            .expect("executable fixture");
        let script = format!(
            "STAGING_SHELL=$$; export STAGING_SHELL; {}",
            write_script(&url::sh_quote(&dest.to_string_lossy()))
        );
        let status = Command::new("sh")
            .args(["-c", &script])
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .status()
            .expect("local shell fixture");
        assert!(!status.success(), "failed transfer cannot report success");
        assert_eq!(
            std::fs::read_to_string(&dest).expect("destination bytes"),
            "old bytes"
        );
        assert!(
            std::fs::read_dir(dir.path())
                .expect("staging directory")
                .all(|entry| !entry
                    .expect("directory entry")
                    .file_name()
                    .to_string_lossy()
                    .contains(".dial-stage-")),
            "staging cleaned after interrupted={interrupted}"
        );
    }
}

#[test]
fn discovered_aliases_stay_opaque_so_openssh_applies_the_full_config() {
    let home = tempfile::tempdir().expect("home fixture");
    let ssh = home.path().join(".ssh");
    std::fs::create_dir(&ssh).expect("ssh directory");
    std::fs::write(
        ssh.join("config"),
        "Host bastion\n\
         \tHostName 10.0.0.9\n\
         \tUser jump\n\
         Host prod\n\
         \tHostName prod.internal\n\
         \tUser deploy\n\
         \tPort 2222\n\
         \tIdentityFile ~/.ssh/id_prod\n\
         \tProxyJump bastion\n",
    )
    .expect("config fixture");
    let mut configured = BTreeMap::new();
    configured.insert(
        "bastion".to_owned(),
        SshHostConfig {
            host: "10.1.1.1".into(),
            user: Some("ops".into()),
            port: Some(2200),
            key: None,
        },
    );
    configured.insert(
        "staging".to_owned(),
        SshHostConfig {
            host: "staging.example".into(),
            user: None,
            port: None,
            key: None,
        },
    );
    let targets = host_targets(home.path(), &configured);
    let settings = Settings {
        user_dir: home.path().join("dial"),
        config: crate::config::tools::ssh::SshConfig::default(),
    };
    // Connection arguments without a ControlMaster, so the test touches no control directory.
    let args = |name: &str| -> Vec<String> {
        let target = &targets[name];
        let mut args = connection::common_args(target, &settings, None, false);
        args.push(target.destination().expect("destination"));
        args
    };
    let has_pair = |args: &[String], flag: &str, value: &str| {
        args.windows(2)
            .any(|pair| pair[0] == flag && pair[1] == value)
    };

    // Discovered aliases stay opaque: no HostName, port, key or user is copied into the arguments,
    // so the ProxyJump and other settings in OpenSSH's configuration keep applying.
    let prod = &targets["prod"];
    assert_eq!(prod.host, "prod");
    assert_eq!(
        (prod.user.as_deref(), prod.port, prod.key.as_ref()),
        (None, None, None)
    );
    let prod_args = args("prod");
    assert_eq!(prod_args.last().map(String::as_str), Some("prod"));
    assert!(!prod_args.iter().any(|arg| arg == "-p" || arg == "-i"));
    assert!(!prod_args.iter().any(|arg| arg.contains("prod.internal")));

    // A configured [ssh_hosts] entry wins over the same discovered alias.
    let bastion_args = args("bastion");
    assert_eq!(
        bastion_args.last().map(String::as_str),
        Some("ops@10.1.1.1")
    );
    assert!(has_pair(&bastion_args, "-p", "2200"));
    assert!(!bastion_args.iter().any(|arg| arg.contains("10.0.0.9")));

    // A configured entry with no discovered alias keeps its explicit HostName.
    let staging_args = args("staging");
    assert_eq!(
        staging_args.last().map(String::as_str),
        Some("staging.example")
    );
    assert!(!staging_args.iter().any(|arg| arg == "-p" || arg == "-i"));
}
