use super::*;

fn dev(allow_network: bool) -> Policy {
    Policy::Dev {
        deny_read: vec![PathBuf::from("/home/u/.ssh")],
        extra_write: vec![PathBuf::from("/home/u/.cargo")],
        allow_network,
    }
}

#[test]
fn dev_with_network_keeps_low_integrity_and_other_policies_use_an_appcontainer() {
    let policy = dev(true);
    let Backend::LowIntegrity {
        deny_read,
        extra_write,
    } = backend(&policy)
    else {
        panic!("dev with network must stay on Low integrity");
    };
    assert_eq!(deny_read, [PathBuf::from("/home/u/.ssh")]);
    assert_eq!(extra_write, [PathBuf::from("/home/u/.cargo")]);
    let offline = dev(false);
    let Backend::AppContainer(plan) = backend(&offline) else {
        panic!("dev without network needs an AppContainer");
    };
    assert!(!plan.network);
    let strict = Policy::Strict {
        deny_read: Vec::new(),
        allow_network: true,
        extra_write: Vec::new(),
    };
    assert!(matches!(backend(&strict), Backend::AppContainer(plan) if plan.network));
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
