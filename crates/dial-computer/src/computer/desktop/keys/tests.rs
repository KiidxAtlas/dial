use super::*;

#[test]
fn computer_parses_full_alias_table() {
    let cases = [
        (
            &["cmd", "META", "super", "win", "windows", "command"][..],
            KeyName::Meta,
        ),
        (&["option", "alt"][..], KeyName::Alt),
        (&["control", "ctrl"][..], KeyName::Ctrl),
        (&["enter", "return"][..], KeyName::Enter),
        (&["esc", "escape"][..], KeyName::Escape),
        (&["delete", "del"][..], KeyName::Delete),
        (&["up", "arrowup"][..], KeyName::Up),
        (&["down", "arrowdown"][..], KeyName::Down),
        (&["left", "arrowleft"][..], KeyName::Left),
        (&["right", "arrowright"][..], KeyName::Right),
        (&["printscreen", "printscr"][..], KeyName::PrintScreen),
    ];
    for (aliases, expected) in cases {
        for alias in aliases {
            assert_eq!(parse_key(alias).unwrap(), expected);
        }
    }
    assert_eq!(
        parse_keys(&["cmd+shift+p".into()]).unwrap(),
        [KeyName::Meta, KeyName::Shift, KeyName::Char('p')]
    );
    assert_eq!(
        parse_key("garbage").unwrap_err().code,
        super::super::error::ErrorCode::InvalidKey
    );
}

#[test]
fn computer_presses_in_order_and_releases_in_reverse_even_after_errors() {
    let keys = [KeyName::Ctrl, KeyName::Shift, KeyName::Char('p')];
    let record = |events: &mut Vec<(KeyName, KeyDirection)>, key, direction| {
        events.push((key, direction));
        Ok::<_, &str>(())
    };
    let mut events = Vec::new();
    hold_keys(
        &mut events,
        keys,
        record,
        |events| {
            events.push((KeyName::Space, KeyDirection::Press));
            Ok(())
        },
        Result::and,
    )
    .unwrap();
    assert_eq!(
        events,
        [
            (KeyName::Ctrl, KeyDirection::Press),
            (KeyName::Shift, KeyDirection::Press),
            (KeyName::Char('p'), KeyDirection::Press),
            (KeyName::Space, KeyDirection::Press),
            (KeyName::Char('p'), KeyDirection::Release),
            (KeyName::Shift, KeyDirection::Release),
            (KeyName::Ctrl, KeyDirection::Release)
        ]
    );

    let mut events = Vec::new();
    let result = hold_keys(
        &mut events,
        keys,
        |events, key, direction| {
            events.push((key, direction));
            match (key, direction) {
                (KeyName::Char('p'), KeyDirection::Press) => Err("press"),
                (KeyName::Ctrl, KeyDirection::Release) => Err("release"),
                _ => Ok(()),
            }
        },
        |_| unreachable!("operation must not run after a failed press"),
        Result::and,
    );
    assert_eq!(result, Err("press"));
    assert_eq!(
        events[3..],
        [
            (KeyName::Shift, KeyDirection::Release),
            (KeyName::Ctrl, KeyDirection::Release)
        ]
    );

    let mut events = Vec::new();
    let result = hold_keys(&mut events, keys, record, |_| Err("operation"), Result::and);
    assert_eq!(result, Err("operation"));
    assert_eq!(events.len(), 6);
}
