use super::*;

#[test]
fn profile_rejects_unsafe_paths() {
    for bad in ["/tmp/a\"b", "/tmp/a\nb", "/tmp/a\\b", "relative/path", "/"] {
        assert!(profile(Path::new(bad), &[], false).is_err(), "{bad:?}");
    }
}

#[test]
fn profile_scopes_access_to_workspace() {
    let text = profile(Path::new("/private/tmp/my ws"), &[], false).unwrap();
    assert!(text.contains("(deny default)"));
    assert!(text.contains("(allow file-read* file-write* (subpath \"/private/tmp/my ws\"))"));
    assert!(!text.contains("network"));
    assert!(!text.contains("/Users"));
    assert!(
        profile(Path::new("/private/tmp/ws"), &[], true)
            .unwrap()
            .contains("(allow network*)")
    );
}

#[test]
fn profile_grants_attached_roots_workspace_access() {
    let extra = [PathBuf::from("/private/tmp/extra root")];
    let text = profile(Path::new("/private/tmp/ws"), &extra, false).unwrap();
    assert!(text.contains("(allow file-read* file-write* (subpath \"/private/tmp/ws\"))"));
    assert!(text.contains("(allow file-read* file-write* (subpath \"/private/tmp/extra root\"))"));
}

#[test]
fn strict_and_dev_profiles_exclude_nested_private_tree_last() {
    let fx = Fixture::new();
    let private = fx.ws.join("state/private");
    std::fs::create_dir_all(&private).unwrap();
    let denied = vec![private.clone()];
    for text in [
        strict_profile(&fx.ws, &denied, std::slice::from_ref(&private), false).unwrap(),
        dev_profile(&fx.ws, &denied, std::slice::from_ref(&private), false).unwrap(),
    ] {
        let rule = format!(
            "(deny file-read* file-write* (subpath {}))\n",
            quote(&private).unwrap()
        );
        assert!(text.ends_with(&rule));
        assert!(text.contains(&format!(
            "(deny file-write-unlink (literal {}))",
            quote(&fx.ws.join("state")).unwrap()
        )));
        assert!(!text.contains("(deny file-write* (subpath"));
    }
}

#[cfg(target_os = "macos")]
#[test]
fn strict_and_dev_deny_nested_private_state_without_blocking_ordinary_files() {
    if !require_for_test() {
        return;
    }
    let fx = Fixture::new();
    let private = fx.ws.join("state/private");
    std::fs::create_dir_all(&private).unwrap();
    std::fs::write(private.join("secret"), "private").unwrap();
    for policy in [
        Policy::Strict {
            deny_read: vec![private.clone()],
            extra_write: vec![],
            allow_network: false,
        },
        Policy::Dev {
            deny_read: vec![private.clone()],
            extra_write: vec![],
            allow_network: false,
        },
    ] {
        let (code, output) = fx.run(
            &policy,
            r#"
                echo ordinary > ordinary && [ "$(cat ordinary)" = ordinary ] || exit 10
                if cat state/private/secret 2>/dev/null; then exit 11; fi
                if echo bad > state/private/new 2>/dev/null; then exit 12; fi
                if mv state moved 2>/dev/null; then exit 13; fi
                echo ok
            "#,
        );
        assert_eq!(code, Some(0), "{output}");
    }
}

/// A repository whose `.git` is a directory with a hooks directory and a `HEAD`.
fn git_dir(root: &Path) -> PathBuf {
    let git = root.join(".git");
    std::fs::create_dir_all(git.join("hooks")).unwrap();
    std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    git
}

#[test]
fn git_metadata_covers_dot_git_linked_worktrees_and_opt_out() {
    let fx = Fixture::new();
    assert!(
        git_metadata(std::slice::from_ref(&fx.ws))
            .unwrap()
            .is_empty()
    );
    let git = git_dir(&fx.ws);
    assert_eq!(
        git_metadata(std::slice::from_ref(&fx.ws)).unwrap(),
        vec![git.clone()]
    );
    // Listing the metadata itself as writable is the explicit opt-out.
    assert!(
        git_metadata(&[fx.ws.clone(), git.join("hooks")])
            .unwrap()
            .is_empty()
    );
    // A linked worktree in `extra` points into the main repository's `.git` in `ws`.
    let linked = git.join("worktrees/feature");
    std::fs::create_dir_all(&linked).unwrap();
    std::fs::write(linked.join("commondir"), "../..\n").unwrap();
    std::fs::write(
        fx.extra.join(".git"),
        format!("gitdir: {}\n", linked.display()),
    )
    .unwrap();
    let both = git_metadata(&[fx.ws.clone(), fx.extra.clone()]).unwrap();
    assert!(both.contains(&git), "{both:?}");
    assert!(both.contains(&fx.extra.join(".git")), "{both:?}");
    assert!(both.contains(&linked), "{both:?}");
    // Metadata outside every writable root is already read-only and is not listed.
    assert_eq!(
        git_metadata(std::slice::from_ref(&fx.extra)).unwrap(),
        vec![fx.extra.join(".git")]
    );
}

#[test]
fn strict_and_dev_profiles_make_git_metadata_read_only_after_write_grants() {
    let fx = Fixture::new();
    let git = git_dir(&fx.ws);
    let rule = format!("(deny file-write* (subpath {}))", quote(&git).unwrap());
    for text in [
        strict_profile(&fx.ws, &[], &[], false).unwrap(),
        dev_profile(&fx.ws, &[], &[], false).unwrap(),
    ] {
        let deny = text.find(&rule).expect("git metadata rule");
        let allow = text.rfind("(allow file-write*").unwrap();
        assert!(allow < deny, "{text}");
        assert!(text.contains(&format!(
            "(deny file-write-unlink (literal {}))",
            quote(&fx.ws).unwrap()
        )));
    }
    let opted_out = dev_profile(&fx.ws, &[], std::slice::from_ref(&git), false).unwrap();
    assert!(!opted_out.contains(&rule));
}

#[cfg(target_os = "macos")]
#[test]
fn strict_and_dev_keep_git_metadata_read_only_inside_the_workspace() {
    if !require_for_test() {
        return;
    }
    let fx = Fixture::new();
    git_dir(&fx.ws);
    for policy in [
        Policy::Strict {
            deny_read: vec![],
            extra_write: vec![],
            allow_network: false,
        },
        Policy::Dev {
            deny_read: vec![],
            extra_write: vec![],
            allow_network: false,
        },
    ] {
        let (code, output) = fx.run(
            &policy,
            r#"
                echo ordinary > ordinary || exit 10
                [ "$(cat .git/HEAD)" = "ref: refs/heads/main" ] || exit 11
                if echo evil > .git/hooks/pre-commit 2>/dev/null; then exit 12; fi
                if echo evil >> .git/HEAD 2>/dev/null; then exit 13; fi
                if mv .git moved 2>/dev/null; then exit 14; fi
                if rm -rf .git 2>/dev/null; then exit 15; fi
                [ -d .git/hooks ] || exit 16
                if mv ../ws ../ws-moved 2>/dev/null; then exit 17; fi
                echo ok
            "#,
        );
        assert_eq!(code, Some(0), "{output}");
    }
}

#[cfg(target_os = "macos")]
#[test]
fn sandboxed_git_init_still_creates_new_metadata() {
    if !require_for_test() {
        return;
    }
    let fx = Fixture::new();
    let (code, output) = fx.run(
        &fx.policy(false),
        "mkdir -p .git/hooks && echo '[core]' > .git/config && echo ok",
    );
    assert_eq!(code, Some(0), "{output}");
}

/// A sandboxed command under filtered network reaches an allowed host only through the proxy:
/// bypassing it, or asking it for a denied host, fails. Uses a local upstream, no internet.
#[cfg(target_os = "macos")]
#[test]
fn filtered_network_allows_only_proxied_permitted_hosts() {
    use std::io::{Read, Write};
    if !require_for_test() {
        return;
    }
    let upstream = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = upstream.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in upstream.incoming().flatten() {
            let mut stream = stream;
            let mut buffer = [0u8; 2048];
            let _ = stream.read(&mut buffer);
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nreached",
            );
        }
    });
    let rules = netproxy::NetworkRules::new(&["127.0.0.1".to_string()], &[])
        .unwrap()
        .unwrap();
    let endpoint = netproxy::endpoint(&rules).unwrap();
    let fx = Fixture::new();
    for policy in [
        fx.policy(true),
        Policy::Strict {
            deny_read: vec![],
            extra_write: vec![],
            allow_network: true,
        },
    ] {
        let script = format!(
            r#"[ "$(/usr/bin/curl -sS --max-time 10 http://127.0.0.1:{port}/)" = reached ] || exit 10
               if /usr/bin/curl -sS --max-time 5 --noproxy '*' http://127.0.0.1:{port}/ 2>/dev/null; then exit 11; fi
               code=$(/usr/bin/curl -s -o /dev/null -w '%{{http_code}}' --max-time 10 http://localhost:{port}/)
               [ "$code" = 403 ] || {{ echo "localhost got $code"; exit 12; }}
               echo ok"#
        );
        let mut command = Sandbox::Available
            .command_routed(
                &fx.ws,
                &policy,
                Some(&endpoint),
                "/bin/bash",
                &["-c", &script],
                None,
            )
            .unwrap();
        command
            .envs(netproxy::client_env(endpoint.client_port()))
            .current_dir(&fx.ws)
            .stdin(Stdio::null());
        let out = command.output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(out.status.code(), Some(0), "{policy:?}: {text}");
    }
}

#[test]
fn unavailable_sandbox_refuses_to_build_commands() {
    let sandbox = Sandbox::Unavailable("nope".to_string());
    assert!(sandbox.available().unwrap_err().contains("nope"));
    assert!(
        sandbox
            .command(Path::new("/private/tmp/ws"), false, "/bin/echo", &[])
            .is_err()
    );
}

#[cfg(target_os = "macos")]
#[test]
fn detect_works_on_macos() {
    if !require_for_test() {
        return;
    }
    assert_eq!(Sandbox::detect(), Sandbox::Available);
}

// ---- Dev policy -------------------------------------------------------

struct Fixture {
    _dir: tempfile::TempDir,
    ws: PathBuf,
    denied: PathBuf,
    extra: PathBuf,
    outside: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let make = |name: &str| {
            let path = base.join(name);
            std::fs::create_dir_all(&path).unwrap();
            path
        };
        let fx = Fixture {
            ws: make("ws"),
            denied: make("denied"),
            extra: make("extra"),
            outside: make("outside"),
            _dir: dir,
        };
        std::fs::write(fx.denied.join("canary"), "secret").unwrap();
        std::fs::write(fx.outside.join("readable"), "visible").unwrap();
        fx
    }

    #[cfg(target_os = "macos")]
    fn policy(&self, allow_network: bool) -> Policy {
        Policy::Dev {
            deny_read: vec![self.denied.clone()],
            extra_write: vec![self.extra.clone()],
            allow_network,
        }
    }

    /// Run `script` in bash under the Dev policy; returns (exit code, stdout+stderr).
    #[cfg(target_os = "macos")]
    fn run(&self, policy: &Policy, script: &str) -> (Option<i32>, String) {
        self.run_with_env(policy, script, std::env::vars_os())
    }

    #[cfg(target_os = "macos")]
    fn run_with_env(
        &self,
        policy: &Policy,
        script: &str,
        env: impl Iterator<Item = (OsString, OsString)>,
    ) -> (Option<i32>, String) {
        let mut command = Sandbox::Available
            .dev_command(&self.ws, policy, "/bin/bash", &["-c", script], env)
            .unwrap();
        command.current_dir(&self.ws).stdin(Stdio::null());
        let out = command.output().unwrap();
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        (out.status.code(), text)
    }
}

#[test]
fn dev_profile_orders_denials_last_and_rejects_unsafe_paths() {
    let fx = Fixture::new();
    let text = dev_profile(
        &fx.ws,
        std::slice::from_ref(&fx.denied),
        std::slice::from_ref(&fx.extra),
        false,
    )
    .unwrap();
    let allow = text.find("(allow file-write*").unwrap();
    let deny = text.find("(deny file-read* file-write* (subpath").unwrap();
    assert!(allow < deny, "denials must come after the write allowances");
    assert!(text.contains("(deny network*)"));
    assert!(
        !dev_profile(&fx.ws, &[], &[], true)
            .unwrap()
            .contains("(deny network*)")
    );
    for bad in [
        "/tmp/a\"b",
        "/tmp/a\nb",
        "/tmp/a\\b",
        "/tmp/a\0b",
        "relative/path",
    ] {
        assert!(
            dev_profile(&fx.ws, &[PathBuf::from(bad)], &[], false).is_err(),
            "deny {bad:?}"
        );
        assert!(
            dev_profile(&fx.ws, &[], &[PathBuf::from(bad)], false).is_err(),
            "write {bad:?}"
        );
    }
    assert!(dev_profile(&fx.ws, &[], &[PathBuf::from("/")], false).is_err());
    assert!(dev_profile(Path::new("/"), &[], &[], false).is_err());
    // A workspace inside a denied path (or equal to it) could never work.
    let parent = fx.ws.parent().unwrap().to_path_buf();
    assert!(dev_profile(&fx.ws, &[parent], &[], false).is_err());
    assert!(dev_profile(&fx.ws, std::slice::from_ref(&fx.ws), &[], false).is_err());
}

#[test]
fn resolve_follows_symlinks_and_dotdot_and_keeps_missing_tails() {
    let fx = Fixture::new();
    let link = fx.ws.join("link");
    std::os::unix::fs::symlink(&fx.denied, &link).unwrap();
    assert_eq!(resolve(&link).unwrap(), fx.denied);
    assert_eq!(
        resolve(&fx.ws.join("..").join("denied")).unwrap(),
        fx.denied
    );
    assert_eq!(
        resolve(&link.join("not").join("yet")).unwrap(),
        fx.denied.join("not").join("yet")
    );
    // `..` after a missing component cannot be resolved lexically.
    assert!(resolve(&fx.ws.join("missing").join("..").join("denied")).is_err());
}

#[test]
fn is_secret_env_flags_credentials_and_keeps_toolchain_vars() {
    for secret in [
        "GITHUB_TOKEN",
        "GH_TOKEN",
        "HF_TOKEN",
        "OPENAI_API_KEY",
        "ANTHROPIC_BASE_URL",
        "AWS_REGION",
        "AWS_PROFILE",
        "MY_SECRET",
        "DB_PASSWORD",
        "STRIPE_KEY",
        "npm_config__authToken",
        "DIAL_HOME",
        "SSH_AUTH_SOCK",
        "GOOGLE_APPLICATION_CREDENTIALS",
        "api_key",
    ] {
        assert!(is_secret_env(secret, "v"), "{secret}");
    }
    for plain in [
        "PATH",
        "HOME",
        "USER",
        "LANG",
        "TERM",
        "SHELL",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "EDITOR",
        "NODE_OPTIONS",
        "PYENV_ROOT",
        "XDG_CONFIG_HOME",
        "DEVELOPER_DIR",
        "HOMEBREW_PREFIX",
        "PWD",
    ] {
        assert!(!is_secret_env(plain, "/some/path"), "{plain}");
    }
    assert!(is_secret_env(
        "HTTPS_PROXY",
        "https://user:hunter2@proxy.example:8080/"
    ));
    assert!(!is_secret_env("HTTPS_PROXY", "https://proxy.example:8080/"));
    assert!(!is_secret_env("REMOTE", "git@github.com:a/b"));
}

#[test]
fn unavailable_sandbox_refuses_dev_commands() {
    let sandbox = Sandbox::Unavailable("nope".to_string());
    let policy = Policy::Dev {
        deny_read: vec![],
        extra_write: vec![],
        allow_network: false,
    };
    assert!(
        sandbox
            .command_with_policy(Path::new("/private/tmp"), &policy, "/bin/echo", &[])
            .is_err()
    );
    let strict = Policy::Strict {
        deny_read: Vec::new(),
        allow_network: false,
        extra_write: Vec::new(),
    };
    assert!(
        sandbox
            .command_with_policy(Path::new("/private/tmp/ws"), &strict, "/bin/echo", &[])
            .is_err()
    );
}

#[cfg(target_os = "macos")]
#[test]
fn dev_reads_widely_but_not_denied_paths() {
    if !require_for_test() {
        return;
    }
    let fx = Fixture::new();
    let policy = fx.policy(false);
    let link_to_denied = fx.ws.join("..").join("denied-link");
    std::os::unix::fs::symlink(&fx.denied, &link_to_denied).unwrap();
    std::os::unix::fs::symlink(fx.denied.join("canary"), fx.ws.join("sneak")).unwrap();
    let script = format!(
        r#"/bin/cat /etc/hosts >/dev/null || exit 20
               [ "$(/bin/cat {outside}/readable)" = visible ] || exit 21
               /bin/ls {outside} >/dev/null || exit 22
               if /bin/cat {denied}/canary 2>/dev/null; then exit 30; fi
               if /bin/cat {denied}/../denied/canary 2>/dev/null; then exit 31; fi
               if /bin/cat {link}/canary 2>/dev/null; then exit 32; fi
               if /bin/cat sneak 2>/dev/null; then exit 33; fi
               if /bin/ls {denied} 2>/dev/null; then exit 34; fi
               if /bin/ln {denied}/canary hardlink 2>/dev/null; then exit 35; fi
               exit 0"#,
        outside = fx.outside.display(),
        denied = fx.denied.display(),
        link = link_to_denied.display(),
    );
    let (code, out) = fx.run(&policy, &script);
    assert_eq!(code, Some(0), "{out}");
}

#[cfg(target_os = "macos")]
#[test]
fn dev_denied_path_given_via_symlink_or_dotdot_is_enforced() {
    let fx = Fixture::new();
    let link = fx.ws.join("alias");
    std::os::unix::fs::symlink(&fx.denied, &link).unwrap();
    for spelled in [link, fx.ws.join("..").join("denied")] {
        let policy = Policy::Dev {
            deny_read: vec![spelled.clone()],
            extra_write: vec![],
            allow_network: false,
        };
        let (code, out) = fx.run(&policy, &format!("/bin/cat {}/canary", fx.denied.display()));
        assert_ne!(code, Some(0), "{spelled:?}: {out}");
        assert!(
            out.contains("Operation not permitted"),
            "{spelled:?}: {out}"
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn dev_confines_writes() {
    if !require_for_test() {
        return;
    }
    let fx = Fixture::new();
    let policy = fx.policy(false);
    let script = format!(
        r#"echo a > in-ws.txt || exit 10
               mkdir sub && echo b > sub/f || exit 11
               echo c > {extra}/f || exit 12
               t=$(/usr/bin/mktemp "$TMPDIR/x.XXXXXX") || exit 13
               case "$t" in {ws}/.tmp/*) ;; *) exit 14;; esac
               echo d > /dev/null || exit 15
               echo e > /dev/stderr || exit 16
               echo f 2>&1 > /dev/stdout || exit 17
               if echo x > {outside}/f 2>/dev/null; then exit 20; fi
               if /bin/mkdir {outside}/d 2>/dev/null; then exit 21; fi
               if /bin/rm {outside}/readable 2>/dev/null; then exit 22; fi
               if echo x > $HOME/.dial-sandbox-test 2>/dev/null; then exit 23; fi
               if echo x > /private/tmp/dial-sandbox-test 2>/dev/null; then exit 24; fi
               if /bin/ln -s {extra}/f {outside}/l 2>/dev/null; then exit 25; fi
               if echo x > {denied}/new 2>/dev/null; then exit 26; fi
               exit 0"#,
        extra = fx.extra.display(),
        ws = fx.ws.display(),
        outside = fx.outside.display(),
        denied = fx.denied.display(),
    );
    let (code, out) = fx.run(&policy, &script);
    assert_eq!(code, Some(0), "{out}");
    assert_eq!(
        std::fs::read_to_string(fx.ws.join("in-ws.txt")).unwrap(),
        "a\n"
    );
    assert_eq!(std::fs::read_to_string(fx.extra.join("f")).unwrap(), "c\n");
    assert!(!fx.outside.join("f").exists());
    assert!(!fx.denied.join("new").exists());
}

#[cfg(target_os = "macos")]
#[test]
fn dev_denial_beats_extra_write_and_workspace() {
    if !require_for_test() {
        return;
    }
    let fx = Fixture::new();
    // A denied dir nested inside the workspace and one inside an extra_write dir.
    let in_ws = fx.ws.join("private");
    let in_extra = fx.extra.join("private");
    for dir in [&in_ws, &in_extra] {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("canary"), "secret").unwrap();
    }
    let policy = Policy::Dev {
        deny_read: vec![in_ws.clone(), in_extra.clone()],
        extra_write: vec![fx.extra.clone()],
        allow_network: false,
    };
    let script = format!(
        r#"echo ok > fine.txt || exit 10
               echo ok > {extra}/fine.txt || exit 11
               for d in {ws}/private {extra}/private; do
                 if /bin/cat $d/canary 2>/dev/null; then exit 20; fi
                 if echo x > $d/new 2>/dev/null; then exit 21; fi
               done
               exit 0"#,
        extra = fx.extra.display(),
        ws = fx.ws.display(),
    );
    let (code, out) = fx.run(&policy, &script);
    assert_eq!(code, Some(0), "{out}");
}

#[cfg(target_os = "macos")]
#[test]
fn dev_network_follows_flag() {
    if !require_for_test() {
        return;
    }
    let fx = Fixture::new();
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let script = format!("(exec 3<>/dev/tcp/127.0.0.1/{port})");
    let (blocked, out) = fx.run(&fx.policy(false), &script);
    assert_ne!(blocked, Some(0), "network must be blocked: {out}");
    let (allowed, out) = fx.run(&fx.policy(true), &script);
    assert_eq!(allowed, Some(0), "network must be allowed: {out}");
}

#[test]
#[cfg(not(windows))]
fn toolchain_caches_redirect_unless_already_writable() {
    let root = PathBuf::from("/home/u/.cache/dial/sandbox");
    let writable = vec![PathBuf::from("/work"), PathBuf::from("/extra")];
    let mut env: Vec<(OsString, OsString)> = [
        ("UV_CACHE_DIR", "/home/u/.cache/uv"),
        ("PIP_CACHE_DIR", "/extra/pip"),
        ("GOCACHE", "relative"),
        ("KEEP_ME", "1"),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v.into()))
    .collect();
    redirect_toolchain_caches(&mut env, &root, &writable);
    let get = |name: &str| {
        let hits: Vec<_> = env.iter().filter(|(k, _)| k == name).collect();
        assert_eq!(hits.len(), 1, "{name} set once: {env:?}");
        hits[0].1.to_string_lossy().into_owned()
    };
    assert_eq!(get("UV_CACHE_DIR"), "/home/u/.cache/dial/sandbox/uv");
    assert_eq!(get("PIP_CACHE_DIR"), "/extra/pip", "writable choice kept");
    assert_eq!(get("GOCACHE"), "/home/u/.cache/dial/sandbox/go-build");
    assert_eq!(get("npm_config_cache"), "/home/u/.cache/dial/sandbox/npm");
    assert_eq!(get("XDG_CACHE_HOME"), "/home/u/.cache/dial/sandbox");
    assert_eq!(get("KEEP_ME"), "1");
}

#[cfg(target_os = "macos")]
#[test]
fn dev_toolchain_caches_are_writable_but_real_caches_stay_read_only() {
    if !require_for_test() {
        return;
    }
    let fx = Fixture::new();
    let home = std::env::var("HOME").unwrap();
    let script = format!(
        "probe=\"$UV_CACHE_DIR/probe-$$\"; mkdir -p \"$UV_CACHE_DIR\" && touch \"$probe\" && rm \"$probe\" && echo cache-ok; \
             touch \"{home}/Library/Caches/dial-sandbox-escape-$$\" 2>/dev/null && echo escaped; \
             echo \"$UV_CACHE_DIR\""
    );
    let (code, out) = fx.run(&fx.policy(false), &script);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("cache-ok"), "{out}");
    assert!(!out.contains("escaped"), "{out}");
    assert!(
        out.contains(&format!("{home}/Library/Caches/dial/sandbox/uv")),
        "{out}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn dev_scrubs_secret_environment() {
    if !require_for_test() {
        return;
    }
    let fx = Fixture::new();
    let env: Vec<(OsString, OsString)> = [
        ("PATH", "/usr/bin:/bin"),
        ("HOME", "/Users/someone"),
        ("KEEP_ME", "1"),
        ("TMPDIR", "/var/should/be/replaced"),
        ("GITHUB_TOKEN", "ghp_x"),
        ("HF_TOKEN", "hf_x"),
        ("OPENAI_API_KEY", "sk-x"),
        ("ANTHROPIC_API_KEY", "sk-ant-x"),
        ("AWS_SECRET_ACCESS_KEY", "aws"),
        ("DB_PASSWORD", "pw"),
        ("PROXY_URL", "http://u:hunter2@host/"),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v.into()))
    .collect();
    let (code, out) = fx.run_with_env(&fx.policy(false), "/usr/bin/env", env.into_iter());
    assert_eq!(code, Some(0), "{out}");
    let lines: Vec<&str> = out.lines().collect();
    assert!(
        !lines.contains(&"KEEP_ME=1"),
        "unknown inherited variable leaked: {out}"
    );
    assert!(lines.contains(&"HOME=/Users/someone"), "{out}");
    assert!(lines.contains(&"PATH=/usr/bin:/bin"), "{out}");
    let tmp = format!(
        "TMPDIR={}",
        fx.ws.join(crate::workspace_copy::SCRATCH_DIR).display()
    );
    assert!(lines.contains(&tmp.as_str()), "{out}");
    for leaked in [
        "ghp_x",
        "hf_x",
        "sk-x",
        "sk-ant-x",
        "aws",
        "hunter2",
        "DB_PASSWORD",
        "should/be/replaced",
    ] {
        assert!(!out.contains(leaked), "{leaked} leaked: {out}");
    }
    // An environment without PATH still gets a usable one.
    let (code, out) = fx.run_with_env(&fx.policy(false), "echo $PATH", std::iter::empty());
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("/usr/bin"), "{out}");
}

#[cfg(target_os = "macos")]
#[test]
fn dev_runs_toolchains() {
    if !require_for_test() {
        return;
    }
    let fx = Fixture::new();
    let script = r#"
            set -e
            bash -c 'echo nested-bash' >/dev/null
            python3 -c 'import json,sys; print(json.dumps([1]))' >/dev/null
            git init -q repo
            cd repo
            echo hi > f.txt
            git add f.txt
            git -c user.name=t -c user.email=t@example.invalid commit -q -m first
            [ "$(git log --format=%s)" = first ]
            cd ..
            for tool in rustc cargo node npm; do
              if command -v "$tool" >/dev/null; then "$tool" --version >/dev/null; fi
            done
            printf 'fn main(){}' > m.rs && rustc m.rs -o m 2>/dev/null || true
        "#;
    let (code, out) = fx.run(&fx.policy(false), script);
    assert_eq!(code, Some(0), "{out}");
    assert!(fx.ws.join("repo/.git").is_dir());
}

#[cfg(target_os = "macos")]
#[test]
fn dev_timeout_kills_the_whole_process_group() {
    if !require_for_test() {
        return;
    }
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    let fx = Fixture::new();
    let marker = fx.ws.join("grandchild-ran");
    let script = format!("(sleep 30; echo late > {}) & wait", marker.display());
    let mut command = Sandbox::Available
        .dev_command(
            &fx.ws,
            &fx.policy(false),
            "/bin/bash",
            &["-c", &script],
            std::env::vars_os(),
        )
        .unwrap();
    command
        .current_dir(&fx.ws)
        .stdin(Stdio::null())
        .process_group(0);
    let mut child = command.spawn().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        child.try_wait().unwrap().is_none(),
        "command should still be running"
    );
    let pgid = i32::try_from(child.id()).unwrap();
    // SAFETY: child was spawned as its own process-group leader and has not been
    // reaped; pgid is its positive PID, so only this fixture group receives SIGKILL.
    assert_eq!(unsafe { libc::killpg(pgid, libc::SIGKILL) }, 0);
    let status = child.wait().unwrap();
    assert_eq!(status.signal(), Some(libc::SIGKILL));
    assert!(!marker.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn probe_policy_reports_enforceable_policies_and_rejects_broken_ones() {
    if !require_for_test() {
        return;
    }
    let fx = Fixture::new();
    for allow_network in [false, true] {
        assert_eq!(
            Sandbox::probe_policy(&fx.ws, &fx.policy(allow_network)),
            Sandbox::Available,
            "dev network={allow_network}"
        );
        assert_eq!(
            Sandbox::probe_policy(
                &fx.ws,
                &Policy::Strict {
                    deny_read: Vec::new(),
                    allow_network,
                    extra_write: Vec::new(),
                },
            ),
            Sandbox::Available,
            "strict network={allow_network}"
        );
    }
    // A policy that cannot be applied to this workspace fails closed.
    let broken = Policy::Dev {
        deny_read: vec![fx.ws.parent().unwrap().to_path_buf()],
        extra_write: vec![],
        allow_network: false,
    };
    match Sandbox::probe_policy(&fx.ws, &broken) {
        Sandbox::Unavailable(reason) => {
            assert!(reason.contains("inside a denied path"), "{reason}")
        }
        Sandbox::Available => panic!("broken policy reported available"),
    }
}

#[cfg(target_os = "macos")]
#[test]
fn strict_policy_through_command_with_policy_matches_command() {
    let fx = Fixture::new();
    let target = fx.outside.join("readable");
    let mut command = Sandbox::Available
        .command_with_policy(
            &fx.ws,
            &Policy::Strict {
                deny_read: Vec::new(),
                allow_network: false,
                extra_write: Vec::new(),
            },
            "/bin/cat",
            &[target.to_str().unwrap()],
        )
        .unwrap();
    let out = command.current_dir(&fx.ws).output().unwrap();
    assert!(
        !out.status.success(),
        "strict policy must not read outside the workspace"
    );
}

#[test]
fn child_environment_is_allowlisted_not_a_secret_name_guess() {
    for name in [
        "PATH",
        "HOME",
        "LANG",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "SystemRoot",
    ] {
        assert!(
            child_env_allowed(name, "/runtime"),
            "runtime variable {name}"
        );
    }
    for name in [
        "DATABASE_DSN",
        "SLACK_WEBHOOK_URL",
        "SERVICE_JWT",
        "OP_SESSION_personal",
        "STRIPE_SK_LIVE",
        "NOT_A_KNOWN_SECRET",
        "NODE_OPTIONS",
        "LD_PRELOAD",
        "BASH_ENV",
        "GIT_CONFIG_COUNT",
    ] {
        assert!(!child_env_allowed(name, "private"), "inherited {name}");
    }
    for value in [
        "https://user@proxy.invalid",
        "https://user:password@proxy.invalid",
        "https://user@proxy.invalid?x",
    ] {
        assert!(
            !child_env_allowed("HTTPS_PROXY", value),
            "proxy userinfo must not be inherited"
        );
    }
    assert!(child_env_allowed(
        "HTTPS_PROXY",
        "https://proxy.invalid:8080"
    ));
}
