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
    // Same name after sanitising: disambiguated by a suffix of its own identity, never replaced.
    assert_eq!(namer.name("fs", "a b"), "mcp__fs__a_b");
    let dotted = namer.name("fs", "a.b");
    let slashed = namer.name("fs", "a/b");
    assert_eq!(
        dotted,
        format!("mcp__fs__a_b_{}", identity_digest("fs", "a.b"))
    );
    assert_eq!(
        slashed,
        format!("mcp__fs__a_b_{}", identity_digest("fs", "a/b"))
    );
    assert_ne!(dotted, slashed);
    // Two servers whose names sanitise alike.
    assert_eq!(namer.name("f.s", "read"), "mcp__f_s__read");
    assert_eq!(
        namer.name("f s", "read"),
        format!("mcp__f_s__read_{}", identity_digest("f s", "read"))
    );

    let long = "x".repeat(100);
    let first = namer.name("srv", &long);
    let second = namer.name("srv", &long);
    let third = namer.name("srv", &long);
    assert_eq!(first.len(), 64);
    assert_eq!(second.len(), 64);
    assert_eq!(third.len(), 64);
    assert_ne!(first, second);
    assert_ne!(second, third);
    assert!(second.ends_with(&identity_digest("srv", &long)));
    for name in [first, second, third] {
        assert!(
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        );
    }
}

#[test]
fn a_colliding_name_does_not_depend_on_registration_order() {
    let mut forward = Namer::default();
    forward.name("fs", "a b");
    forward.name("fs", "a.b");
    let late = forward.name("fs", "a/b");
    let mut reverse = Namer::default();
    reverse.name("fs", "a b");
    let early = reverse.name("fs", "a/b");
    assert_eq!(late, early);
    // A name nothing collides with is unchanged.
    assert_eq!(Namer::default().name("fs", "a/b"), "mcp__fs__a_b");
}
