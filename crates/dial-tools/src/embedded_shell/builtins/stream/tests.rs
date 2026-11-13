use super::super::{Io, RunFn};
use super::*;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn run(f: RunFn, cwd: &Path, args: &[&str], stdin: &str) -> (i32, String, String) {
    let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    let mut input = stdin.as_bytes();
    let mut out: Vec<u8> = Vec::new();
    let mut err: Vec<u8> = Vec::new();
    let code = f(
        &mut Io {
            name: "cmd",
            cwd,
            env: &[],
            synopsis: "",
            stdin: &mut input,
            stdout: &mut out,
            stderr: &mut err,
            declined: None,
        },
        &args,
    );
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

#[test]
fn seq_prints_integers_and_decimals_by_step_and_width() {
    let cases: &[(&[&str], &str)] = &[
        (&["3"], "1\n2\n3\n"),
        (&["2", "4"], "2\n3\n4\n"),
        (&["1", "2", "6"], "1\n3\n5\n"),
        (&["5", "-2", "1"], "5\n3\n1\n"),
        (&["-s,", "3"], "1,2,3\n"),
        (&["-s", " ", "2"], "1 2\n"),
        (&["-w", "8", "11"], "08\n09\n10\n11\n"),
        (&["-w", "-2", "1"], "-2\n-1\n00\n01\n"),
        (&["0.5", "0.5", "2"], "0.5\n1.0\n1.5\n2.0\n"),
        (&["-2", "2"], "-2\n-1\n0\n1\n2\n"),
        (&["3", "1"], ""),
    ];
    for (args, expected) in cases {
        let (code, out, err) = run(seq, Path::new("."), args, "");
        assert_eq!((code, out.as_str()), (0, *expected), "args {args:?}: {err}");
    }
}

#[test]
fn seq_rejects_bad_operands_and_options() {
    let cases: &[(&[&str], i32, &str)] = &[
        (&[], 2, ""),
        (&["1", "2", "3", "4"], 2, ""),
        (&["-q", "3"], 2, ""),
        (&["-s"], 2, ""),
        (&["abc"], 1, "invalid floating point argument: 'abc'"),
        (&["1", "0", "5"], 1, "invalid Zero increment value: '0'"),
        (&["inf"], 1, "invalid floating point argument: 'inf'"),
    ];
    for (args, code, message) in cases {
        let (got, out, err) = run(seq, Path::new("."), args, "");
        assert_eq!(got, *code, "args {args:?}");
        assert_eq!(out, "", "args {args:?}");
        assert!(err.contains(message), "args {args:?}: {err}");
    }
}

#[test]
fn tee_copies_input_to_stdout_and_each_file() {
    let dir = tempdir().unwrap();
    let d = dir.path();
    let (code, out, _) = run(tee, d, &[], "hi\n");
    assert_eq!((code, out.as_str()), (0, "hi\n"));
    let (code, out, _) = run(tee, d, &["f.txt"], "one\n");
    assert_eq!((code, out.as_str()), (0, "one\n"));
    assert_eq!(fs::read_to_string(d.join("f.txt")).unwrap(), "one\n");
    let (code, _, _) = run(tee, d, &["-a", "f.txt"], "two\n");
    assert_eq!(code, 0);
    assert_eq!(fs::read_to_string(d.join("f.txt")).unwrap(), "one\ntwo\n");
    let (code, _, _) = run(tee, d, &["f.txt"], "three\n");
    assert_eq!(code, 0);
    assert_eq!(fs::read_to_string(d.join("f.txt")).unwrap(), "three\n");
}

#[test]
fn tee_reports_unopenable_files_and_still_writes_the_rest() {
    let dir = tempdir().unwrap();
    let d = dir.path();
    let (code, out, err) = run(tee, d, &["missing/x.txt", "ok.txt"], "data\n");
    assert_eq!(code, 1);
    assert_eq!(out, "data\n");
    assert!(err.starts_with("cmd: missing/x.txt: "), "{err}");
    assert_eq!(fs::read_to_string(d.join("ok.txt")).unwrap(), "data\n");
    assert_eq!(run(tee, d, &["-p"], "").0, 2);
}
