#[test]
fn http_clients_get_a_crypto_provider_although_two_backends_are_compiled_in() {
    // Without an installed provider this panics once a second backend (`aws-lc-rs`) is
    // enabled. The default graph uses only `ring`; building a client is what the model worker
    // does first.
    let _agent = crate::llm::agent_with(5, false, None, None);
    assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    let _config = rustls::ClientConfig::builder()
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
}
