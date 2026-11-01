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
fn namespace_prefixes_call_indexes_and_case_resolve_to_one_offered_tool() {
    let catalog = vec!["read".into(), "web_fetch".into(), "Mixed_Tool".into()];
    for (given, want) in [
        ("default_api.read", "read"),
        ("functions.read:0", "read"),
        ("tools.read", "read"),
        ("read:12", "read"),
        (" READ ", "read"),
        ("web-fetch", "web_fetch"),
        ("functions/Web_Fetch:1", "web_fetch"),
        ("mixed_tool", "Mixed_Tool"),
        ("functions.read_file:0", "read"),
    ] {
        assert_eq!(canonical_name(given, &catalog), want, "{given}");
    }
}

#[test]
fn ambiguous_or_unknown_names_are_left_alone_and_exact_names_win() {
    // Two tools differ only by case: nothing is guessed.
    let catalog = vec!["Fetch".into(), "fetch_".into(), "FETCH".into()];
    assert_eq!(canonical_name("fetch", &catalog), "fetch");
    let catalog = vec!["read".into(), "Read".into()];
    assert_eq!(canonical_name("Read", &catalog), "Read");
    assert_eq!(canonical_name("read", &catalog), "read");
    // A colon that is not a numeric call index is not stripped.
    assert_eq!(canonical_name("read:x", &catalog), "read:x");
    assert_eq!(canonical_name("nothing", &catalog), "nothing");
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
fn grep_limit_is_read_as_max_results_unless_both_are_given() {
    let parameters = &json!({"type":"object","properties":{"pattern":{"type":"string"},"max_results":{"type":"integer","minimum":1,"maximum":1000}},"required":["pattern"],"additionalProperties":false});
    let mut args = json!({"pattern":"fn","limit":5});
    normalize("grep", parameters, &mut args);
    assert_eq!(args, json!({"pattern":"fn","max_results":5}));
    assert!(crate::agent::run::calls::validate(parameters, &args).is_ok());
    let mut both = json!({"pattern":"fn","limit":5,"max_results":7});
    normalize("grep", parameters, &mut both);
    assert_eq!(both["max_results"], 7);
    assert!(crate::agent::run::calls::validate(parameters, &both).is_err());
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

#[test]
fn a_list_without_brackets_is_decoded_but_other_tools_strings_stay_errors() {
    let schema = json!({"type":"object","properties":{"query":{"type":"string"},"grep_keywords":{"type":"array","items":{"type":"string"}},"ids":{"type":"array","items":{"type":"integer"}}},"required":["query"],"additionalProperties":false});
    // The JSON text of `["calc", "add"]` without its brackets, as Claude sent it to `find`.
    let mut args = json!({"query":"q","grep_keywords":"calc\", \"add"});
    normalize("find", &schema, &mut args);
    assert_eq!(args["grep_keywords"], json!(["calc", "add"]));
    assert!(crate::agent::run::calls::validate(&schema, &args).is_ok());
    // Outside find's singleton-keyword compatibility, other strings remain errors.
    for text in [
        "calc",
        "DEFAULT_PAGE_SIZE, page_size",
        "say \"hi\"",
        "a\\nb\", 1",
    ] {
        let mut args = json!({"query":"q","grep_keywords":text});
        normalize("extension", &schema, &mut args);
        assert_eq!(args["grep_keywords"], json!(text), "{text}");
        assert!(crate::agent::run::calls::validate(&schema, &args).is_err());
    }
    // A list of non-strings is not decoded this way.
    let mut args = json!({"query":"q","ids":"1\", \"2"});
    normalize("find", &schema, &mut args);
    assert_eq!(args["ids"], json!("1\", \"2"));
}

#[test]
fn find_single_keyword_is_one_literal_not_a_split_list() {
    let schema = json!({"type":"object","properties":{
        "query":{"type":"string"},
        "grep_keywords":{"type":"array","maxItems":32,"items":{"type":"string","minLength":1,"maxLength":256}},
        "paths":{"type":"array","items":{"type":"string"}}
    },"required":["query"],"additionalProperties":false});
    for keyword in [
        "_collect_factor_and_dimension",
        "DEFAULT_PAGE_SIZE, page_size",
        "say \"hi\"",
        "距離",
    ] {
        let mut args = json!({"query":"units", "grep_keywords":keyword});
        normalize("find", &schema, &mut args);
        assert_eq!(args["grep_keywords"], json!([keyword]));
        assert!(crate::agent::run::calls::validate(&schema, &args).is_ok());
    }
    for keyword in [String::new(), "a".repeat(257)] {
        let mut args = json!({"query":"units", "grep_keywords":keyword});
        normalize("find", &schema, &mut args);
        assert!(crate::agent::run::calls::validate(&schema, &args).is_err());
    }
    let mut args = json!({"query":"units", "grep_keywords":"SI"});
    normalize("extension", &schema, &mut args);
    assert_eq!(args["grep_keywords"], "SI");
    let mut args = json!({"query":"units", "paths":"SI"});
    normalize("find", &schema, &mut args);
    assert_eq!(args["paths"], "SI");
    let string_schema = json!({"type":"object","properties":{"grep_keywords":{"type":"string"}}});
    let mut args = json!({"grep_keywords":"SI"});
    normalize("find", &string_schema, &mut args);
    assert_eq!(args["grep_keywords"], "SI");
}
