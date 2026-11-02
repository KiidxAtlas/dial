use super::*;

#[test]
fn quoted_and_escaped_text_keeps_literal_words_and_source_spans() {
    for (command, expected) in [
        (
            r#"echo 'a|b;&&c' "d|e" f\|g"#,
            vec!["echo", "a|b;&&c", "d|e", "f|g"],
        ),
        (r#"echo '' "" x'y'z"#, vec!["echo", "", "", "xyz"]),
        (
            r#"echo "a\q" "a\$b" 'a\b'"#,
            vec!["echo", r"a\q", "a$b", r"a\b"],
        ),
        ("cat 'é;文'", vec!["cat", "é;文"]),
        ("echo 'a\nb'", vec!["echo", "a\nb"]),
    ] {
        let tokens = scan(command).unwrap();
        let words: Vec<_> = tokens
            .iter()
            .map(|token| match &token.kind {
                Kind::Word {
                    value,
                    literal: true,
                } => value.as_str(),
                other => panic!("expected literal words for {command:?}, got {other:?}"),
            })
            .collect();
        assert_eq!(words, expected, "{command:?}");
        for token in tokens {
            assert!(!command[token.span].is_empty(), "{command:?}");
        }
    }
}

#[test]
fn nonliteral_expansions_are_never_confused_with_quoted_or_escaped_text() {
    for (operand, literal) in [
        ("*.rs", false),
        ("-delet?", false),
        ("[-]i", false),
        ("~/key.pem", false),
        ("$HOME", false),
        ("\"$HOME\"", false),
        ("`pwd`", false),
        ("'*.rs'", true),
        ("\"*.rs\"", true),
        (r"\*.rs", true),
        ("'~'", true),
        (r"\~", true),
        ("'$HOME'", true),
        (r"\$HOME", true),
        (r#""\$HOME""#, true),
    ] {
        let tokens = scan(operand).unwrap();
        assert!(
            matches!(tokens.as_slice(), [Token { kind: Kind::Word { literal: got, .. }, .. }] if *got == literal),
            "{operand:?}: {tokens:?}"
        );
    }
}

#[test]
fn every_consumer_loses_boundaries_for_malformed_or_continued_input() {
    for command in [
        "echo 'open",
        "echo \"open",
        "echo trailing\\",
        "echo \"trailing\\",
        "echo a\\\nb",
        "echo \"a\\\nb\"",
        "echo a\\\r\nb",
    ] {
        assert!(scan(command).is_none(), "{command:?}");
        assert!(segments(command, |_| true).is_none(), "{command:?}");
    }
    assert!(scan("echo 'a\\\nb'").is_some());
}

#[test]
fn raw_boundaries_share_quoting_but_keep_operator_grammars_distinct() {
    for (command, pipes, batches) in [
        (
            r#"echo 'a|b;&&c' |& cat x\;y && pwd"#,
            vec![r#"echo 'a|b;&&c' "#, r" cat x\;y && pwd"],
            vec![r#"echo 'a|b;&&c' |& cat x\;y "#, " pwd"],
        ),
        (
            "curl x||echo y|sh",
            vec!["curl x||echo y", "sh"],
            vec!["curl x||echo y|sh"],
        ),
        ("ls;\npwd", vec!["ls;\npwd"], vec!["ls", "", "pwd"]),
        ("ls;;pwd", vec!["ls;;pwd"], vec!["ls;;pwd"]),
        (r"echo a\|b", vec![r"echo a\|b"], vec![r"echo a\|b"]),
        (
            "sh -c 'echo a; sh -c \"echo b|cat\"'; pwd",
            vec!["sh -c 'echo a; sh -c \"echo b|cat\"'; pwd"],
            vec!["sh -c 'echo a; sh -c \"echo b|cat\"'", " pwd"],
        ),
    ] {
        assert_eq!(
            segments(command, |op| matches!(
                op,
                Operator::Pipe | Operator::PipeBoth
            ))
            .unwrap(),
            pipes,
            "pipes: {command:?}"
        );
        assert_eq!(
            segments(command, |op| matches!(
                op,
                Operator::Semicolon | Operator::And | Operator::Newline
            ))
            .unwrap(),
            batches,
            "batches: {command:?}"
        );
    }
}
