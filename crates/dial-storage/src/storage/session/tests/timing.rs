use super::*;

#[test]
fn timing_projection_preserves_messages_and_uses_active_journal_entries() {
    let e = env();
    let mut session = new_session(&e);
    session.append(user("first")).unwrap();
    session.append(assistant("answer")).unwrap();
    session.append(user("abandoned")).unwrap();
    session.append(assistant("abandoned answer")).unwrap();
    session.rewind(1).unwrap();
    session.append(user("replacement")).unwrap();
    let original = session.messages();
    let shown = session.timestamped_messages();
    assert_eq!(shown.len(), original.len());
    for (mut shown, original) in shown.into_iter().zip(&original) {
        let timestamp = shown
            .as_object_mut()
            .unwrap()
            .remove("dial_timestamp")
            .unwrap();
        assert!(timestamp.as_str().unwrap().ends_with('Z'));
        assert_eq!(shown, *original);
    }
    assert_eq!(
        session.messages(),
        original,
        "presentation must not mutate journal messages"
    );
    let reopened = Session::read(session.path().unwrap()).unwrap();
    assert_eq!(
        reopened.timestamped_messages(),
        session.timestamped_messages()
    );
}
