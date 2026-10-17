use super::*;

fn url(input: &str) -> SshUrl {
    match parse(input) {
        Ok(Parsed::Url(url)) => url,
        other => panic!("{input}: {other:?}"),
    }
}

#[test]
fn parses_host_user_port_and_path() {
    let u = url("ssh://deploy@build.example:2222/var/log/app.log");
    assert_eq!(u.user.as_deref(), Some("deploy"));
    assert_eq!(u.host, "build.example");
    assert_eq!(u.port, Some(2222));
    assert_eq!(u.path, RemotePath::Absolute("/var/log/app.log".into()));
    assert_eq!(u.authority, "deploy@build.example:2222");
    let bare = url("ssh://prod/");
    assert_eq!(bare.user, None);
    assert_eq!(bare.port, None);
    assert_eq!(bare.path, RemotePath::Absolute("/".into()));
}

#[test]
fn host_index_and_hostless_paths() {
    assert_eq!(parse("ssh://").unwrap(), Parsed::Index);
    assert_eq!(parse("SSH:///").unwrap(), Parsed::Index);
    assert!(
        parse("ssh:///etc/hosts")
            .unwrap_err()
            .contains("requires a host")
    );
    assert!(parse("ssh://host").unwrap_err().contains("absolute path"));
    assert!(is_ssh_url("Ssh://x/") && !is_ssh_url("ssh:/x") && !is_ssh_url("é"));
}

#[test]
fn keeps_dot_segments_and_decodes_strictly() {
    assert_eq!(
        url("ssh://h/a/../b//c").path,
        RemotePath::Absolute("/a/../b//c".into())
    );
    assert_eq!(
        url("ssh://h/tmp/a%3Fb%23c%3Ad%20e").path,
        RemotePath::Absolute("/tmp/a?b#c:d e".into())
    );
    assert!(parse("ssh://h/tmp/%ZZ").is_err());
    assert!(parse("ssh://h/tmp/%2").is_err());
    assert!(parse("ssh://h/tmp/%00").unwrap_err().contains("NUL"));
    assert!(parse("ssh://h/tmp/%FF").is_err());
}

#[test]
fn home_relative_paths() {
    assert_eq!(url("ssh://h/~").path, RemotePath::Home(String::new()));
    assert_eq!(
        url("ssh://h/~/src/main.rs").path,
        RemotePath::Home("src/main.rs".into())
    );
    assert_eq!(url("ssh://h/~/a b").path.shell_word(), "\"$HOME\"/'a b'");
    assert_eq!(
        url("ssh://h/~foo").path,
        RemotePath::Absolute("/~foo".into())
    );
}

#[test]
fn rejects_queries_fragments_and_bad_authorities() {
    assert!(parse("ssh://h/tmp/a?draft").unwrap_err().contains("%3F"));
    assert!(parse("ssh://h/tmp/a#x").unwrap_err().contains("%23"));
    assert!(parse("ssh://h:/x").unwrap_err().contains("empty port"));
    assert!(parse("ssh://h:0/x").unwrap_err().contains("port 0"));
    assert!(parse("ssh://h:65536/x").is_err());
    assert!(parse("ssh://h:+22/x").is_err());
    assert!(parse("ssh://@h/x").unwrap_err().contains("empty username"));
    assert!(parse("ssh://u:pw@h/x").unwrap_err().contains("password"));
    assert!(parse("ssh://::1/x").unwrap_err().contains("bracketed"));
    assert!(parse("ssh://[::1/x").is_err());
    assert!(parse("ssh://h/x\ny").is_err());
}

#[test]
fn ipv6_literals() {
    let u = url("ssh://root@[fe80::1]:22/etc");
    assert!(u.ipv6);
    assert_eq!(u.host, "fe80::1");
    assert_eq!(u.port, Some(22));
    assert!(parse("ssh://[zz]/x").is_err());
}

#[test]
fn refuses_option_injection_and_whitespace_hosts() {
    assert!(
        parse("ssh://-oProxyCommand=evil/x")
            .unwrap_err()
            .contains("argument-injection")
    );
    assert!(
        parse("ssh://-u@h/x")
            .unwrap_err()
            .contains("argument-injection")
    );
    assert!(
        parse("ssh://%2DoProxyCommand/x")
            .unwrap_err()
            .contains("argument-injection")
    );
    assert!(parse("ssh://a%20b/x").is_err());
    assert!(check_destination_part("host", "prod-1").is_ok());
}

#[test]
fn encoded_aliases_keep_reserved_characters() {
    let u = url("ssh://alice%40prod/etc/hosts");
    assert_eq!(u.authority, "alice@prod");
    assert_eq!(u.user, None);
    assert_eq!(u.host, "alice@prod");
}

#[test]
fn quoting() {
    assert_eq!(sh_quote("it's"), "'it'\\''s'");
    assert_eq!(sh_quote(""), "''");
    assert_eq!(RemotePath::Absolute("/a b".into()).shell_word(), "'/a b'");
}
