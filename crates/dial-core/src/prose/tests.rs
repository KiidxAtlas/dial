use super::words;

#[test]
fn pasted_code_and_identifiers_are_not_request_words() {
    let text = "fix this crash\n  File \"app.py\", line 3, in render_page\n```\nbutton.pack()\n```\ncalled self._build_toolbar()";
    let got: Vec<String> = words(text).collect();
    assert_eq!(
        got,
        ["fix", "this", "crash", "called", "self", "_build_toolbar"]
    );
}
