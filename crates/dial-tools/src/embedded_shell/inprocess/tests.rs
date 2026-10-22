use super::*;

#[test]
fn leaders_are_direct_children_that_lead_their_group() {
    let ps = "  10   1  10\n  11  10  11\n  12  10  10\n  13  11  13\n bad row\n";
    assert_eq!(parse_leaders(ps, 10), vec![11]);
    assert_eq!(parse_leaders(ps, 11), vec![13]);
    assert!(parse_leaders(ps, 99).is_empty());
}

#[cfg(feature = "embedded-shell")]
#[test]
fn run_reports_exit_code_and_writes_output() {
    use std::io::Read;
    let (mut reader, writer) = std::io::pipe().unwrap();
    let mut run = Run::start(Request {
        script: "echo hi; exit 3".into(),
        cwd: std::env::temp_dir(),
        env: vec![("PATH".into(), std::env::var("PATH").unwrap_or_default())],
        output: writer,
    })
    .unwrap();
    let mut out = String::new();
    reader.read_to_string(&mut out).unwrap();
    assert_eq!(out, "hi\n");
    while !run.exited() {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(run.code(), Some(3));
    assert!(run.terminate());
}
