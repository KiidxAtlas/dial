use super::*;

#[test]
fn parses_bun_versions() {
    assert_eq!(parse_version("1.3.14"), Some((1, 3, 14)));
    assert_eq!(parse_version("bun 1.2.9\n"), Some((1, 2, 9)));
    assert_eq!(parse_version("nothing here"), None);
    assert!(parse_version("1.2.9").unwrap() < BUN_MIN);
}
