use super::*;

#[test]
fn slim_aws_credentials_fail_explicitly_without_network_access() {
    let error = match resolve("us-east-1", 1, Arc::new(AtomicBool::new(false))) {
        Ok(_) => panic!("credentials cannot exist without the aws feature"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("`aws` feature"));
}
