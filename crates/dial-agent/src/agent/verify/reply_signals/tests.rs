use super::*;

#[test]
fn classify_unifies_signals_and_ignores_unclosed_code_fences() {
    let signals = ReplySignals::classify("Incomplete: the binary did not build.");
    assert!(!signals.awaiting_input);
    assert_eq!(signals.declared, Some(TurnOutcome::Incomplete));

    let fenced = ReplySignals::classify("```md\nShould I overwrite config.toml?\n");
    assert!(
        !fenced.awaiting_input,
        "unclosed fence must not look like a user question"
    );

    let question = ReplySignals::classify("Both parsers pass.\n\nWhich branch should I keep?");
    assert!(question.awaiting_input);
    assert!(!question.unfinished_closing());
}
