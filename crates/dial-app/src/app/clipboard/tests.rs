use super::*;

#[test]
fn windows_copies_through_powershell_as_utf8() {
    let programs = copy_programs(true);
    assert_eq!(programs.len(), 1);
    let (program, args) = programs[0];
    assert_eq!(program, "powershell.exe");
    let script = args.last().unwrap();
    assert!(script.contains("Set-Clipboard"));
    assert!(script.contains("[Text.Encoding]::UTF8"));
    // No Unix clipboard tool is tried on Windows, and PowerShell is not tried elsewhere.
    assert!(
        copy_programs(false)
            .iter()
            .all(|(program, _)| !program.contains("powershell"))
    );
}
