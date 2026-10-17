use super::super::RunFn;
use super::*;
use std::path::Path;

fn run(f: RunFn, cwd: &Path, args: &[&str], stdin: &str) -> (i32, String, String) {
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    let env = vec![
        ("ZED".to_owned(), "1".to_owned()),
        ("ALPHA".to_owned(), "2".to_owned()),
    ];
    let mut input = std::io::Cursor::new(stdin.as_bytes().to_vec());
    let mut out: Vec<u8> = Vec::new();
    let mut err: Vec<u8> = Vec::new();
    let code = {
        let mut io = Io {
            name: "test",
            cwd,
            env: &env,
            synopsis: "test",
            stdin: &mut input,
            stdout: &mut out,
            stderr: &mut err,
        };
        f(&mut io, &args)
    };
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

fn s(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
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
fn sed_rejects_unsupported_scripts() {
    let cwd = Path::new(".");
    let (code, _, err) = run(sed, cwd, &["d"], "a\n");
    assert_eq!(code, 2);
    assert!(err.contains("unsupported sed script"));
    assert_eq!(run(sed, cwd, &["s/a/\\1/"], "a\n").0, 1);
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

#[test]
fn xargs_batches_words_by_count() {
    let base = s(&["echo", "pre"]);
    assert_eq!(
        batch_args(&base, &["a", "b", "c"], 2),
        vec![s(&["echo", "pre", "a", "b"]), s(&["echo", "pre", "c"])]
    );
    assert_eq!(
        batch_args(&base, &["a", "b", "c"], usize::MAX),
        vec![s(&["echo", "pre", "a", "b", "c"])]
    );
    assert_eq!(batch_args(&base, &[], 2), vec![s(&["echo", "pre"])]);
}

#[test]
fn xargs_replaces_each_input_line() {
    let template = s(&["echo", "[{}]", "{}"]);
    assert_eq!(
        replace_args(&template, "{}", "a b\n\nc\n"),
        vec![s(&["echo", "[a b]", "a b"]), s(&["echo", "[c]", "c"])]
    );
    assert!(replace_args(&template, "{}", "").is_empty());
}
