use super::*;

#[test]
fn aws_signing_preserves_session_token_and_payload_checksum() {
    let credentials = Credentials(aws_credential_types::Credentials::new(
        "AKIDEXAMPLE",
        "test-secret",
        Some("test-session".to_owned()),
        None,
        "dial-test",
    ));
    let headers = credentials
        .sign(
            "us-east-1",
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/example/converse-stream",
            &[("content-type", "application/json")],
            b"{}",
        )
        .unwrap();
    let headers: HashMap<_, _> = headers.into_iter().collect();
    assert_eq!(headers["x-amz-security-token"], "test-session");
    assert_eq!(
        headers["x-amz-content-sha256"],
        "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
    );
    assert!(headers["authorization"].contains("Credential=AKIDEXAMPLE/"));
    assert!(headers["authorization"].contains("/us-east-1/bedrock/aws4_request"));
    let debug = format!("{credentials:?}");
    assert!(!debug.contains("test-secret"));
    assert!(!debug.contains("test-session"));
}
