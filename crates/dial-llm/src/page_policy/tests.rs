use super::*;

const PAGE: &str =
    "<!doctype html><style>a{}</style><script>run()</script><p>x</p><style>b{}</style>";

#[test]
fn every_inline_block_is_hashed_byte_for_byte() {
    let policy = page_policy(PAGE);
    let hash = |body: &str| format!("'sha256-{}'", STANDARD.encode(Sha256::digest(body)));
    assert!(
        policy.starts_with("default-src 'none'; script-src "),
        "{policy}"
    );
    assert!(
        policy.contains(&format!("script-src {};", hash("run()"))),
        "{policy}"
    );
    assert!(
        policy.contains(&format!("style-src {} {};", hash("a{}"), hash("b{}"))),
        "{policy}"
    );
    assert!(policy.ends_with(
        "connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    ));
}

#[test]
fn page_without_inline_blocks_allows_no_inline_code() {
    assert!(
        page_policy("<p>plain</p>")
            .starts_with("default-src 'none'; script-src; style-src; connect-src 'self'")
    );
}

#[test]
fn headers_carry_the_policy_and_no_referrer() {
    assert_eq!(
        page_headers("default-src 'none'"),
        "Content-Security-Policy: default-src 'none'\r\nReferrer-Policy: no-referrer\r\n"
    );
}
