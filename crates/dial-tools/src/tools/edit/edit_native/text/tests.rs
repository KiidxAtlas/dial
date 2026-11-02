use super::*;

#[test]
fn an_untouched_cr_cr_lf_line_keeps_both_carriage_returns() {
    let original = "x\r\r\ny\r\n";
    // The edit pipeline normalizes only CRLF pairs: line 1 becomes content `x\r`.
    let lf = original.replace("\r\n", "\n");
    let after = lf.replace('y', "z");
    assert_eq!(restore_line_endings_like(original, &after), "x\r\r\nz\r\n");
}

#[test]
fn mixed_endings_keep_each_untouched_line_terminator() {
    let original = "a\r\nb\nc\r\n";
    let after = "a\nB\nc\n";
    assert_eq!(
        restore_line_endings_like(original, after),
        "a\r\nB\r\nc\r\n"
    );
}
