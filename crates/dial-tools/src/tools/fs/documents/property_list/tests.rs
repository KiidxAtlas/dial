use super::*;

#[test]
fn binary_plists_render_as_xml() {
    let mut dict = plist::Dictionary::new();
    dict.insert("CFBundleIdentifier".into(), "dev.dial.test".into());
    dict.insert("Count".into(), 3.into());
    let mut bytes = Vec::new();
    plist::Value::Dictionary(dict)
        .to_writer_binary(&mut bytes)
        .unwrap();
    assert!(bytes.starts_with(b"bplist00"));
    let out = read(&bytes).unwrap();
    assert!(out.contains("<key>CFBundleIdentifier</key>"), "{out}");
    assert!(out.contains("<string>dev.dial.test</string>"), "{out}");
    assert!(out.contains("<integer>3</integer>"), "{out}");
    assert!(read(b"bplist00garbage").is_err());
}
