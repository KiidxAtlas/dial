use super::*;

#[test]
fn source_breakpoints_merge_per_line_and_stay_sorted() {
    let list = add_source(&[], json!({"line": 20}));
    let list = add_source(&list, json!({"line": 5}));
    let list = add_source(&list, json!({"line": 20, "condition": "x > 1"}));
    assert_eq!(
        list,
        vec![
            json!({"line": 5}),
            json!({"line": 20, "condition": "x > 1"})
        ]
    );
    let list = remove_source(&list, 5);
    assert_eq!(list, vec![json!({"line": 20, "condition": "x > 1"})]);
    assert!(remove_source(&list, 20).is_empty());
}

#[test]
fn function_breakpoints_merge_by_name() {
    let list = add_function(&[], json!({"name": "main"}));
    let list = add_function(&list, json!({"name": "alpha"}));
    let list = add_function(&list, json!({"name": "main", "condition": "argc > 1"}));
    assert_eq!(
        list,
        vec![
            json!({"name": "alpha"}),
            json!({"name": "main", "condition": "argc > 1"})
        ]
    );
    assert_eq!(
        remove_function(&list, "alpha"),
        vec![json!({"name": "main", "condition": "argc > 1"})]
    );
}

#[test]
fn annotations_round_trip_without_leaking_into_requests() {
    let requested = vec![json!({"line": 3}), json!({"line": 9})];
    let response = json!({"breakpoints": [{"verified": true, "id": 1, "line": 4}, {"verified": false, "message": "no code"}]});
    let annotated = annotate(&requested, &response);
    assert_eq!(
        annotated[0],
        json!({"line": 3, "verified": true, "id": 1, "actualLine": 4})
    );
    assert_eq!(annotated[1]["message"], "no code");
    assert_eq!(request_entries(&annotated), requested);
}
