//! Probe the versioned stdio boundary, not inference. Always use an empty workspace and state.
use dial_client::{Client, ClientError, Launch};
use serde_json::json;
use std::{error::Error, path::PathBuf, time::Duration};

fn main() -> Result<(), Box<dyn Error>> {
    let binary =
        PathBuf::from(std::env::args_os().nth(1).ok_or("expected path to dial")?).canonicalize()?;
    let home = tempfile::tempdir()?;
    let root = tempfile::tempdir()?;
    std::fs::write(
        home.path().join("config.toml"),
        "integrations = []\n[improve]\nenabled = false\n",
    )?;
    let launch = Launch::new(binary)
        .current_dir(root.path())
        .env("DIAL_HOME", home.path())
        .args([
            "--endpoint",
            "http://127.0.0.1:1/v1",
            "--model",
            "offline-fixture",
        ]);
    let wait = Duration::from_secs(30);
    let mut client = Client::connect(&launch, wait)?;
    assert_eq!(client.protocol_version(), 2);
    assert!(client.supports("messages_page"));
    let page = client.call("messages_page", json!({"offset":0}), wait)?;
    assert_eq!(page["messages"], json!([]));
    assert!(
        matches!(client.call("messages_page", json!({"offset":"zero"}), wait),
        Err(ClientError::Rpc { error, .. }) if error.code.as_deref() == Some("invalid_params"))
    );
    assert!(client.shutdown(wait)?.success());
    println!("RPC v2: empty page and invalid_params verified");
    Ok(())
}
