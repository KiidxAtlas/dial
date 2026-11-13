use super::*;

fn os(name: &str, value: &str) -> (OsString, OsString) {
    (name.into(), value.into())
}

fn names(vars: Vec<(OsString, OsString)>) -> Vec<String> {
    vars.into_iter()
        .map(|(name, _)| name.to_string_lossy().into_owned())
        .collect()
}

#[test]
fn windows_system_env_keeps_only_system_variables_in_any_case() {
    let vars = [
        os("SYSTEMROOT", r"C:\Windows"),
        os("windir", r"C:\Windows"),
        os("ComSpec", r"C:\Windows\system32\cmd.exe"),
        os("PATHEXT", ".COM;.EXE;.CMD"),
        os("USERPROFILE", r"C:\Users\a"),
        os("APPDATA", r"C:\Users\a\AppData\Roaming"),
        os("OPENAI_API_KEY", "sk-secret"),
        os("Path", r"C:\bin"),
    ];
    assert_eq!(
        names(windows_system_env_from(vars)),
        ["SYSTEMROOT", "windir", "ComSpec", "PATHEXT"]
    );
    if !cfg!(windows) {
        assert!(windows_system_env().is_empty());
    }
}

#[test]
fn cleared_child_allow_list_adds_windows_names_only_on_windows() {
    const BASE: &[&str] = &["PATH", "HOME"];
    assert_eq!(cleared_child_allow_list(BASE, false), ["PATH", "HOME"]);

    let windows = cleared_child_allow_list(BASE, true);
    for name in [
        "SystemRoot",
        "ComSpec",
        "PATHEXT",
        "TEMP",
        "TMP",
        "USERPROFILE",
        "APPDATA",
    ] {
        assert!(windows.iter().any(|known| known == name), "{name}");
    }
    assert_eq!(
        windows
            .iter()
            .filter(|name| name.eq_ignore_ascii_case("PATH"))
            .count(),
        1
    );
    assert!(
        !windows
            .iter()
            .any(|name| name.contains("KEY") || name.contains("TOKEN"))
    );
}

#[test]
fn allowed_from_passes_only_listed_names_with_case_rules() {
    let allow = cleared_child_allow_list(&["PATH"], true);
    let vars = [
        os("Path", r"C:\bin"),
        os("systemroot", r"C:\Windows"),
        os("OPENAI_API_KEY", "sk-secret"),
        os("TEMP", r"C:\Users\a\AppData\Local\Temp"),
    ];
    assert_eq!(
        names(allowed_from(&allow, vars.clone(), true)),
        ["Path", "systemroot", "TEMP"]
    );
    assert_eq!(names(allowed_from(&allow, vars, false)), ["TEMP"]);
    let exact = allowed_from(
        &["PATH".to_owned()],
        [os("PATH", "/bin"), os("path", "/x")],
        false,
    );
    assert_eq!(names(exact), ["PATH"]);
}
