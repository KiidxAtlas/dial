use super::*;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Loopback OSV: answers every query with `reply` and records each request body.
fn serve(status: u16, reply: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let seen = bodies.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            let body = loop {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break String::new(),
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
                let text = String::from_utf8_lossy(&buf).into_owned();
                let Some((head, body)) = text.split_once("\r\n\r\n") else {
                    continue;
                };
                let length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                if body.len() >= length {
                    break body.to_owned();
                }
            };
            seen.lock().unwrap().push(body);
            let response = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (format!("http://{addr}/v1/query"), bodies)
}

fn stdio(program: &str, args: &[&str]) -> McpServerConfig {
    McpServerConfig {
        program: PathBuf::from(program),
        args: args.iter().map(|arg| arg.to_string()).collect(),
        env: BTreeMap::new(),
        ..McpServerConfig::default()
    }
}

#[test]
fn launched_packages_are_read_from_npx_and_uvx_installs() {
    let read = |program, args: &[&str]| {
        let server = stdio(program, args);
        launched_package(&server).map(|p| {
            (
                p.ecosystem,
                p.name.to_owned(),
                p.version.map(str::to_owned),
                p.whole,
            )
        })
    };
    let some = |e, n: &str, v: Option<&str>, w| Some((e, n.to_owned(), v.map(str::to_owned), w));
    assert_eq!(
        read("npx", &["-y", "@scope/server@1.2.3"]),
        some("npm", "@scope/server", Some("1.2.3"), true)
    );
    assert_eq!(
        read("npx", &["-y", "server@latest", "/root"]),
        some("npm", "server", None, false)
    );
    assert_eq!(
        read("npx", &["-y", "--package=tools@2.0.0", "tools-mcp"]),
        some("npm", "tools", Some("2.0.0"), true)
    );
    assert_eq!(
        read(
            "uvx",
            &["--from", "graphifyy[mcp]==0.4.1", "graphify", "serve"]
        ),
        some("PyPI", "graphifyy", Some("0.4.1"), false)
    );
    assert_eq!(
        read("uvx", &["mcp-git"]),
        some("PyPI", "mcp-git", None, true)
    );
    assert_eq!(read("node", &["server.js"]), None);
    assert_eq!(read("uvx", &["--with", "x"]), None);
}

#[test]
fn only_malicious_advisories_refuse_and_the_query_names_the_package() {
    let (url, bodies) = serve(
        200,
        r#"{"vulns":[{"id":"GHSA-path","database_specific":{"cwe_ids":["CWE-22"]}},{"id":"MAL-2025-123"},{"id":"GHSA-alias","aliases":["MAL-2024-9"]},{"id":"GHSA-cwe","database_specific":{"cwe_ids":["CWE-506"]}}]}"#,
    );
    let server = stdio("npx", &["-y", "evil-pkg@1.0.0"]);
    let why = refusal(&url, "evil", &server).expect("malware advisory refuses");
    assert!(
        why.contains("(MAL-2025-123, GHSA-alias, GHSA-cwe)"),
        "{why}"
    );
    assert!(!why.contains("GHSA-path"), "{why}");
    assert!(
        why.contains("`evil-pkg`") && why.contains("not started"),
        "{why}"
    );
    let sent: Value = serde_json::from_str(&bodies.lock().unwrap()[0]).unwrap();
    assert_eq!(
        sent,
        json!({"package":{"name":"evil-pkg","ecosystem":"npm"},"version":"1.0.0"})
    );

    let (clean, _) = serve(
        200,
        r#"{"vulns":[{"id":"GHSA-path","aliases":["CVE-1"],"database_specific":{"cwe_ids":["CWE-22"]}}]}"#,
    );
    assert_eq!(refusal(&clean, "ok", &server), None);
    let (empty, _) = serve(200, "{}");
    assert_eq!(refusal(&empty, "ok", &server), None);
}

#[test]
fn an_unreachable_or_failing_osv_and_non_package_servers_do_not_refuse() {
    let server = stdio("uvx", &["mcp-git"]);
    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}/v1/query", listener.local_addr().unwrap())
    };
    assert!(malicious_advisories(&closed, "PyPI", "mcp-git", None).is_err());
    assert_eq!(refusal(&closed, "git", &server), None);
    let (failing, _) = serve(500, "{}");
    assert!(malicious_advisories(&failing, "PyPI", "mcp-git", None).is_err());
    assert_eq!(refusal(&failing, "git", &server), None);
    let (url, bodies) = serve(200, r#"{"vulns":[{"id":"MAL-1"}]}"#);
    assert_eq!(refusal(&url, "local", &stdio("node", &["server.js"])), None);
    assert!(
        bodies.lock().unwrap().is_empty(),
        "nothing is sent for a local program"
    );
}
