use super::*;

#[test]
fn switches_and_servers_share_the_lsp_table() {
    let config: LspConfig = toml::from_str(
        "format_on_write = true\n[zig]\nprogram = \"zls\"\nextensions = [\"zig\"]\n[rust-analyzer]\nsettings = { rust-analyzer = { checkOnSave = true } }\n",
    )
    .unwrap();
    assert!(config.diagnostics_on_write && config.format_on_write && !config.start_on_write);
    assert_eq!(config.servers["zig"].program, PathBuf::from("zls"));
    assert!(
        config.servers["rust-analyzer"]
            .program
            .as_os_str()
            .is_empty()
    );
    let round: LspConfig = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
    assert_eq!(round, config);
}

#[test]
fn unknown_switches_are_rejected() {
    assert!(toml::from_str::<LspConfig>("diagnostics = true").is_err());
    assert!(toml::from_str::<LspConfig>("[format_on_write]\nprogram = \"x\"").is_err());
}
