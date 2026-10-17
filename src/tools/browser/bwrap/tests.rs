use super::*;

fn strings(values: &[OsString]) -> Vec<String> {
    values
        .iter()
        .map(|v| v.to_string_lossy().into_owned())
        .collect()
}

#[test]
fn no_network_unshares_net_and_program_follows_separator() {
    let read = [PathBuf::from("/usr")];
    let write = [PathBuf::from("/state/browser")];
    let plan = Plan {
        read_only: &read,
        writable: &write,
        network: false,
        cwd: Path::new("/state/browser"),
    };
    let out = strings(&args(
        &plan,
        Path::new("/usr/bin/node"),
        &[OsString::from("-e")],
    ));
    assert!(out.iter().any(|a| a == "--unshare-net"));
    let sep = out.iter().position(|a| a == "--").expect("separator");
    assert_eq!(&out[sep + 1..], ["/usr/bin/node", "-e"]);
    assert!(
        out.windows(3)
            .any(|w| w == ["--bind", "/state/browser", "/state/browser"])
    );
    assert!(out.windows(3).any(|w| w == ["--ro-bind", "/usr", "/usr"]));
}

#[test]
fn network_mode_keeps_net_namespace_shared_and_never_mounts_workspace() {
    let plan = Plan {
        read_only: &[],
        writable: &[],
        network: true,
        cwd: Path::new("/state/browser"),
    };
    let out = strings(&args(&plan, Path::new("/usr/bin/node"), &[]));
    assert!(!out.iter().any(|a| a == "--unshare-net"));
    assert!(out.iter().any(|a| a == "--die-with-parent"));
    assert!(out.iter().any(|a| a == "--cap-drop"));
    assert!(!out.iter().any(|a| a.contains("workspace")));
}
