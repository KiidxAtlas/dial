use super::*;

fn sel(ranges: &[(usize, Option<usize>)]) -> Selector {
    Selector {
        raw: false,
        ranges: ranges.to_vec(),
        tail: None,
    }
}

fn span(start: usize, end: usize, bounded: bool) -> Span {
    Span {
        start,
        end,
        bounded,
    }
}

#[test]
fn selectors_parse_ranges_tail_and_raw() {
    let (path, s) = split_selector("src/a.rs:50-100").unwrap();
    assert_eq!(
        (path, s.ranges.clone(), s.raw),
        ("src/a.rs", vec![(50, Some(100))], false)
    );
    let (path, s) = split_selector("a.rs:raw:5-9").unwrap();
    assert_eq!((path, s.ranges, s.raw), ("a.rs", vec![(5, Some(9))], true));
    let (path, s) = split_selector("a.rs:5..9:raw").unwrap();
    assert_eq!((path, s.ranges, s.raw), ("a.rs", vec![(5, Some(9))], true));
    assert_eq!(split_selector("a.rs:-30").unwrap().1.tail, Some(30));
    assert_eq!(
        split_selector("a.rs:10+5").unwrap().1.ranges,
        vec![(10, Some(14))]
    );
    assert_eq!(split_selector("a.rs:7-").unwrap().1.ranges, vec![(7, None)]);
    assert_eq!(
        split_selector("a.rs:12,3-5,4-8").unwrap().1.ranges,
        vec![(3, Some(8)), (12, None)]
    );
    assert_eq!(
        split_selector("a.rs:5-16,960-973").unwrap().1.ranges,
        vec![(5, Some(16)), (960, Some(973))]
    );
    let (path, s) = split_selector("archive.zip:inner/file").unwrap();
    assert_eq!((path, s), ("archive.zip:inner/file", Selector::default()));
    assert!(split_selector("a.rs:0").is_err());
    assert!(split_selector("a.rs:9-3").is_err());
    assert!(split_selector("a.rs:-0").is_err());
    assert!(split_selector("a.rs:5+0").is_err());
}

#[test]
fn malformed_numeric_selectors_report_syntax_not_missing_paths() {
    for suffix in ["25- fifty", "25+twenty", "25..end", "25-50,src"] {
        let error = split_selector(&format!("src/a.rs:{suffix}")).unwrap_err();
        assert!(error.contains("Invalid line selector"), "{error}");
        assert!(error.contains("offset/limit"), "{error}");
    }
    assert_eq!(split_selector("a.rs:notes").unwrap().0, "a.rs:notes");
}

#[test]
fn spans_clip_to_the_file_and_drop_ranges_past_the_end() {
    let ranges = sel(&[(2, Some(4)), (8, Some(50)), (90, Some(95))]);
    assert_eq!(
        plan_spans(&ranges, 1, 100, 10),
        vec![span(2, 4, false), span(8, 10, false)]
    );
    assert!(plan_spans(&sel(&[(11, None)]), 1, 100, 10).is_empty());
    assert_eq!(
        plan_spans(&sel(&[(10, None)]), 1, 100, 10),
        vec![span(10, 10, true)]
    );
}

#[test]
fn open_ended_ranges_and_offsets_are_bounded_by_limit() {
    assert_eq!(
        plan_spans(&sel(&[(3, None)]), 1, 4, 100),
        vec![span(3, 6, true)]
    );
    assert_eq!(
        plan_spans(&Selector::default(), 5, 2, 100),
        vec![span(5, 6, true)]
    );
    assert!(plan_spans(&Selector::default(), 5, 2, 4).is_empty());
    assert!(plan_spans(&Selector::default(), 1, 2, 0).is_empty());
    // Unknown length (streaming reads) never overflows.
    assert_eq!(
        plan_spans(&Selector::default(), 1, 3, usize::MAX),
        vec![span(1, 3, true)]
    );
}

#[test]
fn tail_selects_the_last_lines_even_when_longer_than_the_file() {
    let tail = |n| Selector {
        raw: false,
        ranges: vec![],
        tail: Some(n),
    };
    assert_eq!(plan_spans(&tail(3), 1, 10, 10), vec![span(8, 10, false)]);
    assert_eq!(plan_spans(&tail(50), 1, 10, 10), vec![span(1, 10, false)]);
    assert!(plan_spans(&tail(5), 1, 10, 0).is_empty());
}

#[test]
fn footer_states_range_total_and_continuation() {
    let none = Selector::default();
    let raw = Selector {
        raw: true,
        ..Selector::default()
    };
    let explicit = sel(&[(1, Some(5))]);
    assert_eq!(footer("f", &none, &[(1, 5)], 5, None, false), "");
    assert_eq!(
        footer("f", &explicit, &[(1, 5)], 5, None, false),
        "[lines 1-5 of 5]\n"
    );
    assert_eq!(
        footer("f", &none, &[(2, 3)], 5, None, false),
        "[lines 2-3 of 5]\n"
    );
    assert_eq!(
        footer("f", &none, &[(5, 5), (9, 12)], 20, None, false),
        "[lines 5, 9-12 of 20]\n"
    );
    assert_eq!(
        footer("f", &none, &[(1, 2)], 9, Some(3), false),
        "[lines 1-2 of 9; continue with `f:3-`]\n"
    );
    assert_eq!(footer("f", &raw, &[(2, 3)], 5, None, false), "");
    assert_eq!(
        footer("f", &raw, &[(1, 2)], 9, Some(3), false),
        "[continue with `f:3-`]\n"
    );
    assert_eq!(
        footer("f", &none, &[(1, 2)], 9, Some(3), true),
        "[output truncated at the byte budget after line 2 of 9; continue with `f:3-`]\n"
    );
}
