use super::*;

fn dev(allow_network: bool) -> Policy {
    Policy::Dev {
        deny_read: vec![PathBuf::from("/home/u/.ssh")],
        extra_write: vec![PathBuf::from("/home/u/.cargo")],
        allow_network,
    }
}

#[test]
fn only_dev_with_network_is_enforceable() {
    let policy = dev(true);
    let (deny, extra) = enforceable(&policy).unwrap();
    assert_eq!(deny, [PathBuf::from("/home/u/.ssh")]);
    assert_eq!(extra, [PathBuf::from("/home/u/.cargo")]);
    assert!(
        enforceable(&dev(false))
            .unwrap_err()
            .contains("allow_network")
    );
    let strict = Policy::Strict {
        deny_read: Vec::new(),
        allow_network: false,
        extra_write: Vec::new(),
    };
    assert!(enforceable(&strict).unwrap_err().contains("strict"));
}

#[test]
fn writable_roots_inside_protected_paths_are_refused() {
    let denied = [PathBuf::from("/home/u/.config/dial")];
    let inside = [PathBuf::from("/home/u/.config/dial/project")];
    assert!(check_overlap(&inside, &denied).is_err());
    let sibling = [
        PathBuf::from("/home/u/project"),
        PathBuf::from("/home/u/.config/dialx"),
    ];
    assert!(check_overlap(&sibling, &denied).is_ok());
    // A protected path inside a writable root is fine: its explicit label wins over inheritance.
    let nested = [PathBuf::from("/home/u/project/.secrets")];
    assert!(check_overlap(&[PathBuf::from("/home/u/project")], &nested).is_ok());
}

#[test]
fn labels_map_to_mandatory_label_aces() {
    assert_eq!(Label::Writable.ace(true), "(ML;OICI;NW;;;LW)");
    assert_eq!(Label::Writable.ace(false), "(ML;;NW;;;LW)");
    assert_eq!(Label::ReadOnly.ace(true), "(ML;OICI;NW;;;ME)");
    assert_eq!(Label::Hidden.ace(true), "(ML;OICI;NRNW;;;ME)");
    assert_eq!(Label::Hidden.ace(false), "(ML;;NRNW;;;ME)");
}

#[test]
fn command_line_quotes_for_the_c_runtime_parser() {
    let args = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(
        command_line(
            r"C:\Program Files\nodejs\node.exe",
            &args(&["--no-addons", "-e"])
        )
        .unwrap(),
        r#""C:\Program Files\nodejs\node.exe" --no-addons -e"#
    );
    // Spaces and empties are quoted; backslashes are literal unless they precede a quote.
    assert_eq!(
        command_line("p", &args(&["a b", "", r"C:\dir\", r#"say "hi""#, r"a\\b"])).unwrap(),
        r#""p" "a b" "" C:\dir\ "say \"hi\"" a\\b"#
    );
    // A trailing backslash inside quotes is doubled so it does not escape the closing quote.
    assert_eq!(
        command_line("p", &args(&[r"C:\a b\"])).unwrap(),
        r#""p" "C:\a b\\""#
    );
    assert_eq!(
        command_line("p", &args(&[r#"x\"y"#])).unwrap(),
        r#""p" "x\\\"y""#
    );
    assert!(command_line("bad\"prog", &[]).is_err());
}
