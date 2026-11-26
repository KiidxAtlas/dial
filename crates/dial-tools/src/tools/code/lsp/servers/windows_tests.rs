use super::*;

#[test]
fn windows_venv_server_precedes_path() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let bin = root.join(".venv/Scripts");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(root.join("pyproject.toml"), "").unwrap();
    let exe = bin.join("pylsp.exe");
    std::fs::write(&exe, b"fixture").unwrap();
    let found = find_program(Path::new("pylsp"), &[root], root, &[]).unwrap();
    assert_eq!(found.canonicalize().unwrap(), exe.canonicalize().unwrap());
}
