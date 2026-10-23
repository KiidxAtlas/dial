use super::*;
use serde_json::json;

#[test]
fn aliases_are_catalog_scoped_and_real_names_win() {
    let catalog = vec!["read".into(), "bash".into()];
    assert_eq!(canonical_name("functions.read_file", &catalog), "read");
    assert_eq!(canonical_name("shell", &catalog), "bash");
    assert_eq!(canonical_name("search_web", &catalog), "search_web");
    let catalog = vec!["shell".into(), "bash".into()];
    assert_eq!(canonical_name("shell", &catalog), "shell");
}

#[test]
fn aliases_and_scalars_preserve_conflicts_and_bounds() {
    let schema = json!({"type":"object","properties":{"path":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":10}},"required":["path"],"additionalProperties":false});
    let mut args = json!({"file_path":"a.rs","max_lines":"5"});
    normalize("read", &schema, &mut args);
    assert_eq!(args, json!({"path":"a.rs","limit":5}));
    assert!(crate::agent::run::calls::validate(&schema, &args).is_ok());
    let mut args = json!({"path":"a","file_path":"b","limit":"20"});
    normalize("read", &schema, &mut args);
    assert!(args.get("file_path").is_some());
    assert!(crate::agent::run::calls::validate(&schema, &args).is_err());
    let mut args = json!({"path":"a","limit":"20"});
    normalize("read", &schema, &mut args);
    assert!(crate::agent::run::calls::validate(&schema, &args).is_err());
}

#[test]
fn extension_scalars_are_recursive_but_strings_are_not_array_guesses() {
    let schema = json!({"type":"object","properties":{"tasks":{"type":"array","items":{"type":"object","properties":{"investigation":{"type":"boolean"}}}},"evidence":{"type":"array","items":{"type":"string"}}}});
    let mut args = json!({"tasks":[{"investigation":"true"}],"evidence":"a:1"});
    normalize("todo", &schema, &mut args);
    assert_eq!(args["tasks"][0]["investigation"], true);
    assert_eq!(args["evidence"], "a:1");
}

#[test]
fn structural_repair_is_schema_directed_and_preserves_invalid_conflicts() {
    let schema = json!({"type":"object","properties":{
        "path":{"type":"string"}, "limit":{"type":"integer"},
        "tasks":{"type":"array","items":{"type":"object","properties":{
            "enabled":{"type":"boolean"}},"required":["enabled"],"additionalProperties":false}},
        "content":{"type":"string"}},"required":["path"],"additionalProperties":false});
    let mut args = json!({"\"path\"":"a", "limit":null,
        "tasks":"[{\"enabled\":\"true\"}]", "content":"{\"x\":null}"});
    normalize("read", &schema, &mut args);
    assert_eq!(
        args,
        json!({"path":"a","tasks":[{"enabled":true}],"content":"{\"x\":null}"})
    );
    assert!(crate::agent::run::calls::validate(&schema, &args).is_ok());
    for mut args in [
        json!({"path":"a","\"path\"":"b"}),
        json!({"path":null}),
        json!({"path":"a","unknown":null}),
        json!({"path":"a","tasks":"not JSON"}),
    ] {
        normalize("read", &schema, &mut args);
        assert!(
            crate::agent::run::calls::validate(&schema, &args).is_err(),
            "{args}"
        );
    }
    let schema = json!({"type":"object","properties":{"nullable":{"type":["null","string"]}}});
    let mut args = json!({"nullable":null});
    normalize("extension", &schema, &mut args);
    assert!(args.get("nullable").is_some());
}

#[test]
fn common_client_spellings_map_only_onto_declared_properties() {
    let catalog = vec!["web_fetch".into(), "web_search".into(), "edit".into()];
    assert_eq!(canonical_name("WebFetch", &catalog), "web_fetch");
    assert_eq!(
        canonical_name("functions.WebSearch", &catalog),
        "web_search"
    );
    assert_eq!(canonical_name("WebFetch", &["read".into()]), "WebFetch");
    let schema = json!({"type":"object","properties":{"path":{"type":"string"},"old_string":{"type":"string"},"new_string":{"type":"string"},"replace_all":{"type":"boolean"}},"required":["path","old_string","new_string"],"additionalProperties":false});
    let mut args = json!({"filePath":"a","oldString":"x","newString":"y","replaceAll":"true"});
    normalize("edit", &schema, &mut args);
    assert_eq!(
        args,
        json!({"path":"a","old_string":"x","new_string":"y","replace_all":true})
    );
    assert!(crate::agent::run::calls::validate(&schema, &args).is_ok());
    let schema = json!({"type":"object","properties":{"command":{"type":"string"},"cwd":{"type":"string"}},"required":["command"],"additionalProperties":false});
    let mut args = json!({"command":"ls","workdir":"src"});
    normalize("bash", &schema, &mut args);
    assert_eq!(args, json!({"command":"ls","cwd":"src"}));
}
