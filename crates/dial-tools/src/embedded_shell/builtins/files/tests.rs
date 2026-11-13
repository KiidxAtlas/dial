use super::super::{Io, RunFn};
use super::*;
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

fn write(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
}

#[test]
fn basename_strips_directories_and_suffix() {
    let dir = tempdir().unwrap();
    let d = dir.path();
    assert_eq!(
        run(basename, d, &["a/b.txt", ".txt"], ""),
        (0, "b\n".into(), String::new())
    );
    assert_eq!(run(basename, d, &["x", "x"], "").1, "x\n");
    assert_eq!(run(basename, d, &["dir/file/"], "").1, "file\n");
    assert_eq!(run(basename, d, &[], "").0, 2);
}

#[test]
fn dirname_follows_gnu() {
    assert_eq!(dirname_of("a/b/"), "a");
    assert_eq!(dirname_of("a/b"), "a");
    assert_eq!(dirname_of("a"), ".");
    assert_eq!(dirname_of("/"), "/");
    assert_eq!(dirname_of("/a"), "/");
    assert_eq!(dirname_of("a//b"), "a");
}

#[test]
fn head_limits_lines_and_handles_short_inputs() {
    let dir = tempdir().unwrap();
    let d = dir.path();
    write(&d.join("n.txt"), "1\n2\n3\n4\n");
    write(&d.join("short.txt"), "only\n");
    assert_eq!(run(head, d, &["-n", "2", "n.txt"], "").1, "1\n2\n");
    assert_eq!(run(head, d, &["-n", "5", "short.txt"], "").1, "only\n");
    assert_eq!(run(head, d, &["-n1"], "a\nb\n").1, "a\n");
}

#[test]
fn head_and_tail_print_headers_for_several_files() {
    let dir = tempdir().unwrap();
    let d = dir.path();
    write(&d.join("a"), "x\n");
    write(&d.join("b"), "y\n");
    assert_eq!(
        run(head, d, &["a", "b"], "").1,
        "==> a <==\nx\n\n==> b <==\ny\n"
    );
    assert_eq!(
        run(tail, d, &["a", "b"], "").1,
        "==> a <==\nx\n\n==> b <==\ny\n"
    );
}

#[test]
fn tail_keeps_last_lines_including_unterminated_one() {
    let dir = tempdir().unwrap();
    let d = dir.path();
    assert_eq!(run(tail, d, &["-n", "2"], "a\nb\nc").1, "b\nc");
    assert_eq!(run(tail, d, &["-n", "1"], "a\nb\n").1, "b\n");
    assert_eq!(run(tail, d, &["-n", "0"], "a\n").1, "");
    assert_eq!(run(tail, d, &["-n", "9"], "a\nb").1, "a\nb");
}

#[test]
fn missing_file_is_reported_and_exit_is_one() {
    let dir = tempdir().unwrap();
    assert_eq!(
        run(tail, dir.path(), &["nope.txt"], ""),
        (
            1,
            String::new(),
            "cmd: nope.txt: No such file or directory\n".into()
        )
    );
}

#[test]
fn cat_numbers_lines_and_reads_stdin() {
    let dir = tempdir().unwrap();
    assert_eq!(
        run(cat, dir.path(), &["-n"], "a\nb\n").1,
        "     1\ta\n     2\tb\n"
    );
    assert_eq!(run(cat, dir.path(), &[], "xy").1, "xy");
}

#[test]
fn wc_counts_lines_words_and_bytes() {
    let dir = tempdir().unwrap();
    let d = dir.path();
    assert_eq!(
        run(wc, d, &[], "one two\nthree\n").1,
        format!("{:>7}{:>7}{:>7}\n", 2, 3, 14)
    );
    assert_eq!(run(wc, d, &["-l"], "a\nb\n").1, "      2\n");
    write(&d.join("f.txt"), "a b\n");
    write(&d.join("g.txt"), "c\n");
    assert_eq!(run(wc, d, &["-w", "f.txt"], "").1, "      2 f.txt\n");
    assert_eq!(
        run(wc, d, &["-l", "f.txt", "g.txt"], "").1,
        "      1 f.txt\n      1 g.txt\n      2 total\n"
    );
}

#[test]
fn glob_supports_star_question_and_classes() {
    assert!(glob_match("*.rs", "lib.rs"));
    assert!(!glob_match("*.rs", "lib.rs.bak"));
    assert!(glob_match("?.txt", "a.txt"));
    assert!(!glob_match("?.txt", "ab.txt"));
    assert!(glob_match("[ab]1", "b1"));
    assert!(!glob_match("[!ab]1", "b1"));
    assert!(glob_match("[a-c]*", "cat"));
    assert!(!glob_match("[a-c]*", "dog"));
}

#[test]
fn head_bytes_keeps_leading_bytes_across_forms() {
    let dir = tempdir().unwrap();
    let d = dir.path();
    write(&d.join("t.txt"), "abcdef\nghi\n");
    assert_eq!(run(head, d, &["-c", "4", "t.txt"], "").1, "abcd");
    assert_eq!(run(head, d, &["-c3"], "xyz123").1, "xyz");
    assert_eq!(run(head, d, &["-c", "99"], "short").1, "short");
    assert_eq!(run(head, d, &["-c", "0"], "abc").1, "");
}

#[test]
fn head_and_tail_accept_the_dash_number_shorthand() {
    let dir = tempdir().unwrap();
    let d = dir.path();
    assert_eq!(run(head, d, &["-1"], "a\nb\nc\n").1, "a\n");
    assert_eq!(run(head, d, &["-2", "-"], "a\nb\nc\n").1, "a\nb\n");
    assert_eq!(run(tail, d, &["-1"], "a\nb\nc\n").1, "c\n");
    assert_eq!(run(head, d, &["-x"], "a\n").0, 2);
    assert_eq!(run(head, d, &["-+1"], "a\n").0, 2);
}

#[test]
fn head_bytes_stops_reading_an_endless_input() {
    struct Endless;
    impl Read for Endless {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            buf.fill(b'y');
            Ok(buf.len())
        }
    }
    let dir = tempdir().unwrap();
    let mut input = Endless;
    let mut out: Vec<u8> = Vec::new();
    let mut err: Vec<u8> = Vec::new();
    let code = head(
        &mut Io {
            name: "head",
            cwd: dir.path(),
            env: &[],
            synopsis: "",
            stdin: &mut input,
            stdout: &mut out,
            stderr: &mut err,
            declined: None,
        },
        &["-c".to_owned(), "1000000".to_owned()],
    );
    assert_eq!(code, 0);
    assert_eq!(out.len(), 1_000_000);
}

#[test]
fn head_bytes_leaves_the_rest_of_stdin_unread() {
    let dir = tempdir().unwrap();
    let mut input: &[u8] = b"abcdefgh";
    let mut out: Vec<u8> = Vec::new();
    let mut err: Vec<u8> = Vec::new();
    let code = head(
        &mut Io {
            name: "head",
            cwd: dir.path(),
            env: &[],
            synopsis: "",
            stdin: &mut input,
            stdout: &mut out,
            stderr: &mut err,
            declined: None,
        },
        &["-c".to_owned(), "2".to_owned()],
    );
    assert_eq!(code, 0);
    assert_eq!(out, b"ab");
    assert_eq!(input, &b"cdefgh"[..]);
}

#[test]
fn rm_force_ignores_missing_paths() {
    let dir = tempdir().unwrap();
    let d = dir.path();
    assert_eq!(
        run(rm, d, &["-f", "missing"], ""),
        (0, String::new(), String::new())
    );
    assert_eq!(
        run(rm, d, &["missing"], ""),
        (
            1,
            String::new(),
            "cmd: missing: No such file or directory\n".into()
        )
    );
    fs::create_dir(d.join("sub")).unwrap();
    assert_eq!(run(rm, d, &["sub"], "").0, 1);
    assert_eq!(run(rm, d, &["-r", "sub"], "").0, 0);
    assert!(!d.join("sub").exists());
}

#[test]
fn mkdir_p_creates_parents_and_accepts_existing() {
    let dir = tempdir().unwrap();
    let d = dir.path();
    assert_eq!(run(mkdir, d, &["-p", "a/b/c"], "").0, 0);
    assert!(d.join("a/b/c").is_dir());
    assert_eq!(run(mkdir, d, &["-p", "a/b/c"], "").0, 0);
    assert_eq!(
        run(mkdir, d, &["a/b/c"], ""),
        (1, String::new(), "cmd: a/b/c: File exists\n".into())
    );
}

#[test]
fn cp_copies_into_existing_directory() {
    let dir = tempdir().unwrap();
    let d = dir.path();
    write(&d.join("s.txt"), "data\n");
    fs::create_dir(d.join("dest")).unwrap();
    assert_eq!(run(cp, d, &["s.txt", "dest"], "").0, 0);
    assert_eq!(fs::read_to_string(d.join("dest/s.txt")).unwrap(), "data\n");

    fs::create_dir(d.join("tree")).unwrap();
    write(&d.join("tree/f.txt"), "x");
    let (code, _, err) = run(cp, d, &["tree", "dest"], "");
    assert_eq!(code, 1);
    assert!(err.contains("omitting directory"));
    assert_eq!(run(cp, d, &["-r", "tree", "dest"], "").0, 0);
    assert!(d.join("dest/tree/f.txt").is_file());
}

#[test]
fn ls_hides_dotfiles_unless_all() {
    let dir = tempdir().unwrap();
    let d = dir.path();
    write(&d.join("b"), "");
    write(&d.join("a"), "");
    write(&d.join(".hidden"), "");
    assert_eq!(run(ls, d, &[], "").1, "a\nb\n");
    assert_eq!(run(ls, d, &["-a"], "").1, ".\n..\n.hidden\na\nb\n");
}

#[test]
fn mv_moves_into_existing_directory() {
    let dir = tempdir().unwrap();
    let d = dir.path();
    write(&d.join("m.txt"), "moved");
    fs::create_dir(d.join("dest")).unwrap();
    assert_eq!(run(mv, d, &["m.txt", "dest"], "").0, 0);
    assert!(!d.join("m.txt").exists());
    assert_eq!(fs::read_to_string(d.join("dest/m.txt")).unwrap(), "moved");
}

#[test]
fn touch_creates_missing_files() {
    let dir = tempdir().unwrap();
    let d = dir.path();
    assert_eq!(run(touch, d, &["new.txt"], "").0, 0);
    assert!(d.join("new.txt").is_file());
}
