use super::*;
#[test]
fn identical_tagged_notices_shrink_but_user_text_is_untouched() {
    let text = "diagnostic ".repeat(100);
    let user = json!({"role":"user","content":text});
    let notice = json!({"role":"user","content":text,"dial_reminder":"reasoning"});
    let mut messages = vec![user.clone(), notice.clone(), notice.clone()];
    project(
        &mut messages,
        &mut Supersession::default(),
        &Policy {
            warm_tokens: 0,
            pressure: false,
            protect_tokens: 0,
            supersede_reads: true,
            drop_useless: true,
        },
    );
    assert_eq!(
        messages,
        vec![user.clone(), notice.clone(), notice.clone()],
        "calm prefixes remain reusable"
    );
    project(
        &mut messages,
        &mut Supersession::default(),
        &Policy {
            warm_tokens: 0,
            pressure: true,
            protect_tokens: 0,
            supersede_reads: true,
            drop_useless: true,
        },
    );
    assert_eq!(messages[0], user);
    assert!(messages[1]["content"].as_str().unwrap().len() < 100);
    assert_eq!(messages[2], notice);
}
