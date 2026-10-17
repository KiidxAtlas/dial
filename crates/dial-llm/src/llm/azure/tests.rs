use super::*;
#[test]
fn deployment_scoped_v1_endpoint_and_query_encoding() {
    assert_eq!(
        url("https://resource.openai.azure.com/openai/v1").unwrap(),
        "https://resource.openai.azure.com/openai/v1/responses?api-version=v1"
    );
    assert_eq!(percent_encode("2025-01-01 preview"), "2025-01-01%20preview");
}
