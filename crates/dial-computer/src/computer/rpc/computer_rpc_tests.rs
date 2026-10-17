use super::*;

#[test]
fn computer_error_payload_round_trip() {
    let error = ComputerError::abort("stopped");
    let payload = error.to_payload();
    assert_eq!(ComputerError::from_payload(&payload), error);
}

#[test]
fn computer_parse_null_args_as_empty_object() {
    let parsed: ListApplicationsArgs = parse("listApplications", Value::Null).unwrap();
    assert!(parsed.options.is_none());
}
