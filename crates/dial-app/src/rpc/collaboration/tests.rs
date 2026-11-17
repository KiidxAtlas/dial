use super::*;

#[test]
fn join_requires_explicit_consent_and_validates_without_contacting_relay() {
    let link = protocol::format_link(
        "wss://relay.example.com",
        "AAAAAAAAAAAAAAAAAAAAAA",
        &[5; 32],
        &[6; 32],
        None,
    )
    .unwrap();
    for params in [
        json!({"link":link}),
        json!({"link":link,"confirm":false}),
        json!({"link":link,"confirm":"true"}),
        json!({"link":link,"confirm":true,"unknown":1}),
        json!({"link":link,"confirm":true,"name":"\n"}),
        json!({"link":"secret-invalid-link","confirm":true}),
    ] {
        let error = request(&params, "").unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<super::super::RequestError>()
                .unwrap()
                .code,
            ErrorCode::InvalidParams
        );
        assert!(!error.to_string().contains("secret-invalid-link"));
    }
    let parsed = request(&json!({"link":link,"confirm":true}), "").unwrap();
    assert_eq!(parsed.name, "guest");
    // Cancellation is tested at the actual owner boundary, before a socket can be created.
    let error = join(
        &Config::default(),
        &json!({"link":link,"confirm":true}),
        &AtomicBool::new(true),
    )
    .unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<super::super::RequestError>()
            .unwrap()
            .code,
        ErrorCode::Conflict
    );
}

#[test]
fn guest_requests_never_fall_through_admission_or_run_locally() {
    let _room = TEST_ROOM.lock().unwrap();
    use crate::collab::guest::Guest;
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = room::leave();
        }
    }
    let _cleanup = Cleanup;
    let challenge = json!({"t":"challenge","nonce":"test-challenge"});
    let granted = json!({"t":"welcome","proto":protocol::COLLAB_PROTO,"readOnly":false});
    let read_only = json!({"t":"welcome","proto":protocol::COLLAB_PROTO,"readOnly":true});
    for (frames, allowed) in [
        (vec![], false),
        (vec![challenge.clone()], false),
        (vec![challenge.clone(), read_only], false),
        (vec![challenge.clone(), granted.clone()], true),
        (vec![challenge, granted, json!({"t":"bye"})], false),
    ] {
        room::with(|slot| {
            assert!(slot.is_none());
            *slot = Some(room::Room::Guest(Guest::fixture(&frames)));
        });
        let result = prompt("send", &json!({"text":"do not execute locally"}))
            .expect("guest routing must claim every request");
        assert_eq!(result.is_ok(), allowed, "frames: {frames:?}");
        if let Ok(value) = result {
            assert_eq!(value["local_turn"], false);
        }
        assert_eq!(
            abort().expect("guest abort never falls through").is_ok(),
            allowed
        );
        assert!(prompt("retry", &json!({})).unwrap().is_err());
        assert!(
            prompt(
                "send",
                &json!({"text":"with image","images":["private.png"]})
            )
            .unwrap()
            .is_err()
        );
        assert_eq!(leave().unwrap()["left"], true);
        assert!(prompt("send", &json!({"text":"local after explicit leave"})).is_none());
    }
}
