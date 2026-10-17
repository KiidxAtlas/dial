use super::*;

fn parse_text(text: &str) -> Vec<DiscoveredHost> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    let dir = Path::new("/nonexistent/.ssh");
    parse(
        text,
        &dir.join("config"),
        Path::new("/nonexistent"),
        dir,
        0,
        &mut seen,
        &mut out,
    );
    out
}

#[test]
fn lists_concrete_aliases_with_first_values() {
    let hosts = parse_text(
        "# comment\n\
             Host prod staging *.internal !bad\n\
             \tHostName %h.example.com\n\
             \tUser deploy\n\
             \tPort 2222\n\
             \tIdentityFile ~/.ssh/id_prod\n\
             Host prod\n\
             \tUser ignored\n\
             \tProxyJump bastion\n\
             Host=bastion\n\
             HostName = 10.0.0.1\n\
             Host *\n\
             \tUser everyone\n",
    );
    let names: Vec<_> = hosts.iter().map(|h| h.alias.as_str()).collect();
    assert_eq!(names, ["prod", "staging", "bastion"]);
    assert_eq!(hosts[0].hostname.as_deref(), Some("prod.example.com"));
    assert_eq!(hosts[1].hostname.as_deref(), Some("staging.example.com"));
    assert_eq!(hosts[0].user.as_deref(), Some("deploy"));
    assert_eq!(hosts[0].port, Some(2222));
    assert_eq!(hosts[0].identity_file.as_deref(), Some("~/.ssh/id_prod"));
    assert_eq!(hosts[2].hostname.as_deref(), Some("10.0.0.1"));
    assert_eq!(hosts[2].user, None);
}

#[test]
fn match_blocks_and_quotes() {
    let hosts = parse_text(
        "Host \"quoted name\" plain # trailing comment\n\
             Match host plain\n\
             \tUser matched\n\
             Host plain\n\
             \tPort notanumber\n",
    );
    assert_eq!(hosts.len(), 2);
    assert_eq!(hosts[0].alias, "quoted name");
    assert_eq!(hosts[1].user, None);
    assert_eq!(hosts[1].port, None);
}

#[test]
fn follows_includes_relative_to_ssh_dir_with_globs() {
    let home = tempfile::tempdir().unwrap();
    let ssh = home.path().join(".ssh");
    fs::create_dir_all(ssh.join("conf.d")).unwrap();
    fs::write(
        ssh.join("config"),
        "Include conf.d/*.conf\nHost main\n  User me\nInclude config\n",
    )
    .unwrap();
    fs::write(ssh.join("conf.d/b.conf"), "Host beta\n  Port 2200\n").unwrap();
    fs::write(
        ssh.join("conf.d/a.conf"),
        "Host alpha\nInclude ~/.ssh/extra\n",
    )
    .unwrap();
    fs::write(ssh.join("conf.d/skip.txt"), "Host skipped\n").unwrap();
    fs::write(ssh.join("extra"), "Host gamma\n").unwrap();
    let hosts = discover(home.path());
    let names: Vec<_> = hosts.iter().map(|h| h.alias.as_str()).collect();
    assert_eq!(names, ["alpha", "gamma", "beta", "main"]);
    assert_eq!(hosts[2].port, Some(2200));
    assert_eq!(hosts[3].user.as_deref(), Some("me"));
    assert!(hosts[2].source.ends_with("conf.d/b.conf"));
}

#[test]
fn missing_config_discovers_nothing() {
    let home = tempfile::tempdir().unwrap();
    assert!(discover(home.path()).is_empty());
}

#[test]
fn configured_hosts_merge_and_win_on_alias_clash() {
    let home = tempfile::tempdir().unwrap();
    let ssh = home.path().join(".ssh");
    fs::create_dir_all(&ssh).unwrap();
    fs::write(ssh.join("config"), "Host alpha\n  Port 2200\nHost beta\n").unwrap();
    let mut configured = std::collections::BTreeMap::new();
    configured.insert(
        "alpha".to_owned(),
        crate::config::types::SshHostConfig {
            host: "10.0.0.5".into(),
            user: Some("deploy".into()),
            port: Some(22),
            key: None,
        },
    );
    configured.insert(
        "gamma".to_owned(),
        crate::config::types::SshHostConfig {
            host: "gamma.example".into(),
            user: None,
            port: None,
            key: Some("/keys/g".into()),
        },
    );
    let hosts = resolve(home.path(), &configured);
    let names: Vec<_> = hosts.iter().map(|h| h.alias.as_str()).collect();
    assert_eq!(names, ["alpha", "beta", "gamma"]);
    assert_eq!(hosts[0].hostname.as_deref(), Some("10.0.0.5"));
    assert_eq!(hosts[0].port, Some(22));
    assert_eq!(hosts[0].user.as_deref(), Some("deploy"));
    assert_eq!(hosts[2].identity_file.as_deref(), Some("/keys/g"));
    assert!(hosts[2].source.as_os_str().is_empty());
}
