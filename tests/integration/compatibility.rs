//! Public compatibility paths remain usable while callers migrate to their canonical owners.
use dial::runtime::template;
use serde_json::json;

#[test]
fn public_partial_registration_preserves_the_previous_value_on_parse_failure() {
    let name = "dial-public-compatibility-greeting";
    template::register_partial(name, "hello {{who}}").expect("register public partial");
    let compiled = template::compile(&format!("{{{{> {name}}}}}!"))
        .expect("compile template through the public facade");
    let context = json!({"who": "reader"});
    assert_eq!(
        template::render_system_prompt(&compiled, &context).expect("render registered partial"),
        "hello reader!"
    );
    assert!(template::register_partial(name, "{{#if broken}}").is_err());
    assert_eq!(
        template::render_system_prompt(&compiled, &context).expect("render retained partial"),
        "hello reader!"
    );
    template::register_partial(name, "goodbye {{who}}").expect("replace public partial");
    assert_eq!(
        template::render_system_prompt(&compiled, &context).expect("render replacement"),
        "goodbye reader!"
    );
}

#[test]
fn settings_publication_aliases_retain_the_canonical_private_write_contract() {
    use dial::{config::settings_store, platform::atomic};

    let directory = tempfile::tempdir().expect("private compatibility test directory");
    settings_store::validate_private_path(directory.path()).expect("validate through legacy path");
    atomic::validate_private_path(directory.path()).expect("validate through canonical path");
    let path = directory.path().join("settings.json");
    settings_store::write_private(&path, b"legacy").expect("write through legacy path");
    assert_eq!(std::fs::read(&path).expect("read legacy write"), b"legacy");
    atomic::write_private(&path, b"canonical").expect("replace through canonical path");
    assert_eq!(
        std::fs::read(&path).expect("read canonical write"),
        b"canonical"
    );
}

#[test]
#[expect(
    deprecated,
    reason = "exercise the intentionally retained migration path"
)]
fn acquisition_alias_keeps_canonical_types_and_offline_behavior() {
    use dial::{learn::download as legacy, llm::download};

    let family: download::Family = legacy::default_family_for(false);
    assert_eq!(family, download::default_family_for(false));
    let legacy_family: legacy::Family = download::default_family_for(true);
    assert_eq!(legacy_family, download::default_family_for(true));
}
