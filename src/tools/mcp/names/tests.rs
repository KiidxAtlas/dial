use super::*;

#[test]
fn sanitizes_to_the_allowed_alphabet() {
    assert_eq!(sanitize("my server.v2/tool"), "my_server_v2_tool");
    assert_eq!(sanitize("ok-name_1"), "ok-name_1");
    assert_eq!(sanitize("héllo"), "h_llo");
}

#[test]
fn names_are_prefixed_bounded_and_unique() {
    let mut namer = Namer::default();
    assert_eq!(namer.name("fs", "read"), "mcp__fs__read");
    // Same name after sanitising: disambiguated, never replaced.
    assert_eq!(namer.name("fs", "a b"), "mcp__fs__a_b");
    assert_eq!(namer.name("fs", "a.b"), "mcp__fs__a_b_2");
    assert_eq!(namer.name("fs", "a/b"), "mcp__fs__a_b_3");
    // Two servers whose names sanitise alike.
    assert_eq!(namer.name("f.s", "read"), "mcp__f_s__read");
    assert_eq!(namer.name("f s", "read"), "mcp__f_s__read_2");

    let long = "x".repeat(100);
    let first = namer.name("srv", &long);
    let second = namer.name("srv", &long);
    assert_eq!(first.len(), 64);
    assert_eq!(second.len(), 64);
    assert_ne!(first, second);
    assert!(second.ends_with("_2"));
    for name in [first, second] {
        assert!(
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        );
    }
}
