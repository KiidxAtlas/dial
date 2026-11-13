use super::super::RunFn;
use super::*;
use std::io::Read;
use std::path::Path;

fn run(f: RunFn, cwd: &Path, args: &[&str], stdin: &str) -> (i32, String, String) {
    run_with(
        f,
        cwd,
        args,
        &mut std::io::Cursor::new(stdin.as_bytes().to_vec()),
    )
}

fn run_with(
    f: RunFn,
    cwd: &Path,
    args: &[&str],
    stdin: &mut (dyn Read + Send),
) -> (i32, String, String) {
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    let env = vec![
        ("ZED".to_owned(), "1".to_owned()),
        ("ALPHA".to_owned(), "2".to_owned()),
    ];
    let mut out: Vec<u8> = Vec::new();
    let mut err: Vec<u8> = Vec::new();
    let code = {
        let mut io = Io {
            name: "test",
            cwd,
            env: &env,
            synopsis: "test",
            stdin,
            stdout: &mut out,
            stderr: &mut err,
            declined: None,
        };
        let code = f(&mut io, &args);
        // As the dispatcher reports a declined command when `PATH` has no program of its name.
        if let Some(message) = io.declined.take() {
            let _ = writeln!(io.stderr, "{}: {message}", io.name);
        }
        code
    };
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

#[test]
fn cut_selects_ranges_and_keeps_delimiterless_lines() {
    let cwd = Path::new(".");
    let input = "a:b:c\nplain\nw:x:y:z\n";
    assert_eq!(run(cut, cwd, &["-d:", "-f2"], input).1, "b\nplain\nx\n");
    assert_eq!(
        run(cut, cwd, &["-d", ":", "-f", "1,3"], input).1,
        "a:c\nplain\nw:y\n"
    );
    assert_eq!(run(cut, cwd, &["-d:", "-f2-"], "a:b:c\n").1, "b:c\n");
    assert_eq!(run(cut, cwd, &["-d:", "-f-2"], "a:b:c\n").1, "a:b\n");
    assert_eq!(run(cut, cwd, &["-f2"], "x\ty\n").1, "y\n");
}

#[test]
fn cut_rejects_bad_arguments() {
    let cwd = Path::new(".");
    assert_eq!(run(cut, cwd, &["-d:"], "a\n").0, 2);
    let (code, _, err) = run(cut, cwd, &["-d::", "-f1"], "a\n");
    assert_eq!(code, 1);
    assert!(err.contains("single character"));
    assert_eq!(run(cut, cwd, &["-f0"], "a\n").0, 1);
}

#[test]
fn field_list_parses_all_forms() {
    assert_eq!(
        parse_field_list("1,3-4,6-,-2").unwrap(),
        vec![(1, 1), (3, 4), (6, usize::MAX), (1, 2)]
    );
    assert!(parse_field_list("0").is_err());
    assert!(parse_field_list("4-2").is_err());
    assert!(parse_field_list("-").is_err());
}

#[test]
fn grep_reports_matches_and_exit_codes() {
    let cwd = Path::new(".");
    let input = "abc\nxyz\nbb\n";
    let (code, out, _) = run(grep, cwd, &["b"], input);
    assert_eq!((code, out.as_str()), (0, "abc\nbb\n"));
    assert_eq!(
        run(grep, cwd, &["q"], input),
        (1, String::new(), String::new())
    );
    assert_eq!(run(grep, cwd, &["-v", "b"], input).1, "xyz\n");
    assert_eq!(run(grep, cwd, &["-n", "b"], input).1, "1:abc\n3:bb\n");
    assert_eq!(run(grep, cwd, &["-c", "b"], input).1, "2\n");
    assert_eq!(run(grep, cwd, &["-i", "ABC"], input).1, "abc\n");
    assert_eq!(run(grep, cwd, &["("], input).0, 2);
}

#[test]
fn grep_prefixes_file_names_and_lists_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hit\nmiss\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "miss\n").unwrap();
    let cwd = dir.path();
    assert_eq!(
        run(grep, cwd, &["hit", "a.txt", "b.txt"], "").1,
        "a.txt:hit\n"
    );
    assert_eq!(
        run(grep, cwd, &["-l", "hit", "a.txt", "b.txt"], "").1,
        "a.txt\n"
    );
    assert_eq!(
        run(grep, cwd, &["-c", "miss", "a.txt", "b.txt"], "").1,
        "a.txt:1\nb.txt:1\n"
    );
    let (code, _, err) = run(grep, cwd, &["hit", "gone.txt"], "");
    assert_eq!(code, 2);
    assert!(err.contains("gone.txt: No such file or directory"));
}

#[test]
fn grep_recurses_in_sorted_order() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("d/sub")).unwrap();
    std::fs::write(dir.path().join("d/sub/b.txt"), "needle\n").unwrap();
    std::fs::write(dir.path().join("d/a.txt"), "needle\nno\n").unwrap();
    assert_eq!(
        run(grep, dir.path(), &["-r", "needle", "d"], "").1,
        "d/a.txt:needle\nd/sub/b.txt:needle\n"
    );
}

#[test]
fn grep_matches_extended_fixed_and_repeated_patterns() {
    let cwd = Path::new(".");
    let input = "abc\nxyz\nqq a.c\n";
    let cases: &[(&[&str], &str)] = &[
        (&["-E", "b|x"], "abc\nxyz\n"),
        (&["-F", "a.c"], "qq a.c\n"),
        (&["-F", "-i", "A.C"], "qq a.c\n"),
        (&["-e", "b", "-e", "q"], "abc\nqq a.c\n"),
        (&["-e", "x\nq"], "xyz\nqq a.c\n"),
        (&["-E", "-e", "z"], "xyz\n"),
    ];
    for &(args, expected) in cases {
        assert_eq!(run(grep, cwd, args, input).1, expected, "grep {args:?}");
    }
    assert_eq!(run(grep, cwd, &["-F", "("], "f(x\nfx\n").1, "f(x\n");
    assert_eq!(run(grep, cwd, &["-F", "-e", "("], input).0, 1);
}

#[test]
fn grep_rejects_invalid_options_and_patterns() {
    let cwd = Path::new(".");
    let (code, _, err) = run(grep, cwd, &["-P", "a"], "a\n");
    assert_eq!(code, 2);
    assert!(err.contains("invalid option -- 'P'"));
    let (code, _, err) = run(grep, cwd, &["-e"], "a\n");
    assert_eq!(code, 2);
    assert!(err.contains("option requires an argument -- 'e'"));
    assert_eq!(run(grep, cwd, &["-E", "("], "a\n").0, 2);
    assert_eq!(run(grep, cwd, &["-e", "a", "-e", "("], "a\n").0, 2);
    assert_eq!(run(grep, cwd, &[], "a\n").0, 2);
}

#[test]
fn grep_quiet_only_sets_status() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hit\n").unwrap();
    let cwd = dir.path();
    assert_eq!(
        run(grep, cwd, &["-q", "hit", "a.txt"], ""),
        (0, String::new(), String::new())
    );
    assert_eq!(run(grep, cwd, &["-q", "miss", "a.txt"], "").0, 1);
    // A selected line gives status 0 even after an unreadable file, as in GNU grep.
    assert_eq!(run(grep, cwd, &["-q", "hit", "gone.txt", "a.txt"], "").0, 0);
}

#[test]
fn grep_only_matching_and_file_name_flags() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "abcab\nxyz\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "ab\n").unwrap();
    let cwd = dir.path();
    assert_eq!(run(grep, cwd, &["-o", "b", "a.txt"], "").1, "b\nb\n");
    assert_eq!(
        run(grep, cwd, &["-on", "ab", "a.txt"], "").1,
        "1:ab\n1:ab\n"
    );
    // Empty matches print nothing, though the line still counts as selected.
    assert_eq!(
        run(grep, cwd, &["-o", "q*", "a.txt"], ""),
        (0, String::new(), String::new())
    );
    assert_eq!(run(grep, cwd, &["-o", "-v", "b", "a.txt"], "").1, "");
    assert_eq!(
        run(grep, cwd, &["-h", "ab", "a.txt", "b.txt"], "").1,
        "abcab\nab\n"
    );
    assert_eq!(
        run(grep, cwd, &["-H", "ab", "a.txt"], "").1,
        "a.txt:abcab\n"
    );
    assert_eq!(run(grep, cwd, &["-Hc", "ab", "a.txt"], "").1, "a.txt:1\n");
}

#[test]
fn sed_substitutes_with_groups_and_escapes() {
    let cwd = Path::new(".");
    assert_eq!(run(sed, cwd, &["s/a/X/"], "aaa\n").1, "Xaa\n");
    assert_eq!(run(sed, cwd, &["s/a/X/g"], "aaa\n").1, "XXX\n");
    assert_eq!(run(sed, cwd, &["-n", "s/a/X/p"], "a\nb\n").1, "X\n");
    assert_eq!(
        run(sed, cwd, &["s/([a-z])([0-9])/\\2\\1/g"], "a1 b2\n").1,
        "1a 2b\n"
    );
    assert_eq!(run(sed, cwd, &["s/b/[&]/"], "abc\n").1, "a[b]c\n");
    assert_eq!(run(sed, cwd, &["s/b/\\&/"], "abc\n").1, "a&c\n");
    assert_eq!(run(sed, cwd, &["s/a\\/b/X/"], "a/b c\n").1, "X c\n");
    assert_eq!(run(sed, cwd, &["s/x/\\//"], "x\n").1, "/\n");
    assert_eq!(run(sed, cwd, &["-e", "s/a/b/"], "a\n").1, "b\n");
}

#[test]
fn sed_prints_and_deletes_by_address() {
    let cwd = Path::new(".");
    let cases: &[(&[&str], &str, &str)] = &[
        (&["-n", "2p"], "a\nb\nc\n", "b\n"),
        (&["2d"], "a\nb\nc\n", "a\nc\n"),
        (&["-n", "$p"], "a\nb\nc\n", "c\n"),
        (&["$d"], "a\nb\nc\n", "a\nb\n"),
        (&["2,$d"], "a\nb\nc\n", "a\n"),
        (&["-n", "2,1p"], "a\nb\nc\n", "b\n"),
        (&["-n", "3,10p"], "a\nb\nc\n", "c\n"),
        (&["-n", "5p"], "a\nb\nc\n", ""),
        (&["/b/d"], "a\nb\nc\n", "a\nc\n"),
        (&["-n", "/a/,/b/p"], "a\nb\nc\n", "a\nb\n"),
        (&["-n", "/b/,$p"], "a\nb\nc\n", "b\nc\n"),
        (&["/b/,/c/d"], "a\nb\nx\nc\nd\n", "a\nd\n"),
        (&["1d;$d"], "a\nb\nc\n", "b\n"),
        (&["/x/d;1,2p"], "a\nx\nb\n", "a\na\nb\n"),
        (&["-n", "p"], "a\nb\n", "a\nb\n"),
        (&["p"], "a\nb\n", "a\na\nb\nb\n"),
    ];
    for &(args, input, expected) in cases {
        assert_eq!(run(sed, cwd, args, input).1, expected, "sed {args:?}");
    }
    let numbers: String = (1..=25).map(|n| format!("{n}\n")).collect();
    let first_twenty: String = (1..=20).map(|n| format!("{n}\n")).collect();
    assert_eq!(run(sed, cwd, &["-n", "1,20p"], &numbers).1, first_twenty);
}

#[test]
fn sed_numbers_lines_across_inputs() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "a1\na2\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "b1\nb2\n").unwrap();
    let cwd = dir.path();
    assert_eq!(run(sed, cwd, &["-n", "3p", "a.txt", "b.txt"], "").1, "b1\n");
    assert_eq!(run(sed, cwd, &["-n", "$p", "a.txt", "b.txt"], "").1, "b2\n");
    let (code, out, err) = run(sed, cwd, &["-n", "$p", "gone.txt", "b.txt"], "");
    assert_eq!((code, out.as_str()), (1, "b2\n"));
    assert!(err.contains("gone.txt: No such file or directory"));
}

#[test]
fn sed_substitution_flags_and_script_separators() {
    let cwd = Path::new(".");
    let cases: &[(&[&str], &str, &str)] = &[
        (&["s/a/X/2"], "aaa\n", "aXa\n"),
        (&["s/a/X/2g"], "aaaa\n", "aXXX\n"),
        (&["s/A/x/i"], "Abc\n", "xbc\n"),
        (&["-n", "s/a/X/gp"], "aa\nb\n", "XX\n"),
        (&["s/q/X/"], "a\n", "a\n"),
        (&["s/b/X/;s/a/Y/"], "ab\n", "YX\n"),
        (&["-e", "1d", "-e", "s/b/X/"], "a\nb\n", "X\n"),
    ];
    for &(args, input, expected) in cases {
        assert_eq!(run(sed, cwd, args, input).1, expected, "sed {args:?}");
    }
}

#[test]
fn sed_rejects_unsupported_scripts() {
    let cwd = Path::new(".");
    for script in [
        "y/a/b/",
        "s/a/b/w out",
        "1,p",
        "0p",
        "$!d",
        "/x",
        "a foo",
        "s//X/",
        "//d",
    ] {
        let (code, _, err) = run(sed, cwd, &[script], "a\n");
        assert_eq!(
            (code, err.as_str()),
            (2, "test: unsupported sed script\n"),
            "{script:?}"
        );
    }
    assert_eq!(run(sed, cwd, &["/(/d"], "a\n").0, 2);
    assert_eq!(run(sed, cwd, &["s/(/x/"], "a\n").0, 2);
    assert_eq!(run(sed, cwd, &["s/a/\\1/"], "a\n").0, 1);
    assert_eq!(run(sed, cwd, &[], "a\n").0, 2);
}

#[test]
fn sort_orders_bytes_numbers_and_duplicates() {
    let cwd = Path::new(".");
    assert_eq!(run(sort, cwd, &[], "b\na\nB\n").1, "B\na\nb\n");
    assert_eq!(
        run(sort, cwd, &["-n"], "10\n9\nx\n-1\n").1,
        "-1\nx\n9\n10\n"
    );
    assert_eq!(run(sort, cwd, &["-n"], "1b\n1a\n").1, "1a\n1b\n");
    assert_eq!(run(sort, cwd, &["-r"], "a\nc\nb\n").1, "c\nb\na\n");
    assert_eq!(run(sort, cwd, &["-u"], "a\nb\na\nb\n").1, "a\nb\n");
    assert_eq!(run(sort, cwd, &["-n", "-u"], "1\n1.0\n2\n").1, "1\n2\n");
}

#[test]
fn numeric_key_reads_leading_number() {
    assert_eq!(numeric_key("  -3.5x"), -3.5);
    assert_eq!(numeric_key("+7 apples"), 7.0);
    assert_eq!(numeric_key("x"), 0.0);
    assert_eq!(numeric_key("-0"), 0.0);
}

#[test]
fn tr_maps_ranges_deletes_and_repeats_set2() {
    let cwd = Path::new(".");
    assert_eq!(run(tr, cwd, &["a-z", "A-Z"], "abc xyz\n").1, "ABC XYZ\n");
    assert_eq!(
        run(tr, cwd, &["-d", "aeiou"], "hello world\n").1,
        "hll wrld\n"
    );
    assert_eq!(run(tr, cwd, &["abc", "x"], "abcd\n").1, "xxxd\n");
    assert_eq!(run(tr, cwd, &["\\n", " "], "a\nb\n").1, "a b ");
    assert_eq!(run(tr, cwd, &["a-c"], "x").0, 2);
}

#[test]
fn tr_set_expansion_handles_ranges_and_literal_dashes() {
    assert_eq!(expand_set("a-c").unwrap(), vec!['a', 'b', 'c']);
    assert_eq!(expand_set("a-").unwrap(), vec!['a', '-']);
    assert_eq!(expand_set("\\-x").unwrap(), vec!['-', 'x']);
    assert!(expand_set("z-a").is_err());
}

#[test]
fn uniq_collapses_runs_with_counts_and_filters() {
    let cwd = Path::new(".");
    let input = "a\na\nb\nb\nb\nc\n";
    assert_eq!(run(uniq, cwd, &[], input).1, "a\nb\nc\n");
    assert_eq!(
        run(uniq, cwd, &["-c"], input).1,
        "      2 a\n      3 b\n      1 c\n"
    );
    assert_eq!(run(uniq, cwd, &["-d"], input).1, "a\nb\n");
    assert_eq!(run(uniq, cwd, &["-u"], input).1, "c\n");
    assert_eq!(
        run(uniq, cwd, &["-c", "-d"], input).1,
        "      2 a\n      3 b\n"
    );
    assert_eq!(run(uniq, cwd, &["a", "b"], input).0, 2);
}

#[test]
fn env_prints_sorted_assignments() {
    assert_eq!(run(env, Path::new("."), &[], "").1, "ALPHA=2\nZED=1\n");
}

/// A reader that yields one matching line and then fails, so any read past that line is an error.
struct OneLineThenFail {
    sent: bool,
}

impl Read for OneLineThenFail {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.sent {
            return Err(std::io::Error::other("read past the matching line"));
        }
        self.sent = true;
        let line = b"hit\n";
        let n = line.len().min(buf.len());
        buf[..n].copy_from_slice(&line[..n]);
        Ok(n)
    }
}

#[test]
fn grep_quiet_stops_reading_at_first_match() {
    let mut stdin = OneLineThenFail { sent: false };
    let (code, out, err) = run_with(grep, Path::new("."), &["-q", "hit"], &mut stdin);
    assert_eq!((code, out.as_str(), err.as_str()), (0, "", ""));
}
