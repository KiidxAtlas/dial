use super::*;
#[test]
fn windows_argument_quoting_round_trips_core_cases() {
    assert_eq!(quote(&[]), vec![34, 34]);
    assert_eq!(quote(&[97, 32, 98]), vec![34, 97, 32, 98, 34]);
    assert_eq!(quote(&[97, 34, 98]), vec![34, 97, 92, 34, 98, 34]);
    assert_eq!(quote(&[97, 92, 92]), vec![97, 92, 92]);
}
#[test]
fn environment_keys_merge_case_insensitively() {
    let block = env_block(&[("Path".into(), "a".into()), ("PATH".into(), "b".into())]).unwrap();
    assert_eq!(block, vec![80, 65, 84, 72, 61, 98, 0, 0]);
}
#[test]
fn windows_exit_status_preserves_exit_code() {
    assert!(ExitStatus::from_raw(0).success());
    assert_eq!(ExitStatus::from_raw(17).code(), Some(17));
}
