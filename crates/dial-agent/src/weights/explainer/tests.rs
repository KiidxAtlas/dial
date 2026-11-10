use super::*;

#[test]
fn ratings_are_read_from_the_last_array_of_the_right_length() {
    assert_eq!(ratings_in("[1, 2.5, 10]", 3), Some(vec![1.0, 2.5, 10.0]));
    assert_eq!(
        ratings_in("Here you go:\n```json\n[0, 3, 7]\n```", 3),
        Some(vec![0.0, 3.0, 7.0])
    );
    // A bracketed aside before the answer is skipped.
    assert_eq!(
        ratings_in("snippet [2] looks odd. [4, 5]", 2),
        Some(vec![4.0, 5.0])
    );
    assert_eq!(ratings_in("[1, 2]", 3), None);
    assert_eq!(ratings_in("[\"high\", \"low\"]", 2), None);
    assert_eq!(ratings_in("no array", 1), None);
}

#[test]
fn one_part_asks_for_a_description_then_ratings_under_it_and_the_control() {
    let request = json!({"describe": "Describe it.", "rate": "Rate for \"{description}\".",
                         "count": 2, "control": "a token in ordinary text"});
    let mut prompts = Vec::new();
    let mut replies = vec![
        "list separators".to_owned(),
        "not json".to_owned(),
        "[9, 1]".to_owned(),
        "[5, 5]".to_owned(),
    ]
    .into_iter();
    let mut ask = |prompt: &str, retry: Option<&str>| -> Result<String> {
        prompts.push((prompt.to_owned(), retry.is_some()));
        Ok(replies.next().unwrap())
    };
    let answer = one(&mut ask, &request).unwrap();
    assert_eq!(
        answer,
        json!({"explanation": "list separators", "ratings": [9.0, 1.0], "control_ratings": [5.0, 5.0]})
    );
    assert_eq!(
        prompts,
        [
            ("Describe it.".to_owned(), false),
            ("Rate for \"list separators\".".to_owned(), false),
            ("Rate for \"list separators\".".to_owned(), true),
            ("Rate for \"a token in ordinary text\".".to_owned(), false),
        ]
    );
}

#[test]
fn unreadable_ratings_fail_the_part_after_one_retry() {
    let request = json!({"describe": "d", "rate": "r {description}", "count": 2});
    let mut calls = 0;
    let mut ask = |_: &str, _: Option<&str>| -> Result<String> {
        calls += 1;
        Ok(if calls == 1 { "x".into() } else { "[1]".into() })
    };
    let error = one(&mut ask, &request).unwrap_err();
    assert!(format!("{error:#}").contains("2 ratings"));
    assert_eq!(calls, 3);
}

#[test]
fn the_analyzed_model_is_the_first_choice_and_self_needs_no_model() {
    let config = Config::default();
    let choices = choices(&config);
    assert_eq!(choices[0].value, SELF);
    assert!(selected(&config, &json!({})).unwrap().is_none());
    assert!(
        selected(&config, &json!({"explainer": "@self"}))
            .unwrap()
            .is_none()
    );
    assert!(selected(&config, &json!({"explainer": ""})).is_err());
    assert!(selected(&config, &json!({"explainer": 3})).is_err());
}
