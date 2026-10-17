use super::*;
#[test]
fn removes_code_and_markdown() {
    assert_eq!(
        speakable("Hello **world**. `code`\n```rs\nignored()\n```", 200),
        "Hello world. code"
    );
}

#[test]
fn keeps_identifiers_and_unclosed_brackets() {
    assert_eq!(
        speakable("call my_fn_name() and 2*3", 200),
        "call my_fn_name() and 2*3"
    );
    assert_eq!(
        speakable("see [note here\nnext line", 200),
        "see [note here next line"
    );
    assert_eq!(
        speakable("a [link](https://x.y) and ![img](p.png) end", 200),
        "a link and  end"
    );
}
