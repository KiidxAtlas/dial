use super::*;
use crate::storage::artifact::ArtifactStore;
use crate::tools::testkit;
use std::fs;

fn args(value: Value) -> Args {
    serde_json::from_value(value).unwrap()
}
fn run(ws: &Workspace, args: Args) -> Result<ToolOutput> {
    super::run(ws, args, &AtomicBool::new(false))
}
fn parse(bytes: &[u8], format: &str, sheet: Option<&str>) -> Result<table::Table> {
    table::parse(bytes, format, sheet, &AtomicBool::new(false))
}
fn calculate(bytes: &[u8], format: &str, value: Value) -> Result<Value> {
    let table = parse(bytes, format, None)?;
    query::calculate(&table, &args(value))
}

#[test]
fn automatic_formats_expand_handles_without_changing_source_identity() {
    let (work, ws) = testkit::workspace();
    for (name, bytes, format) in [
        ("table.CSV", "x\n7\n", "csv"),
        ("table.JSON", "[{\"x\":7}]", "json"),
        ("table.JSONL", "{\"x\":7}\n", "jsonl"),
    ] {
        fs::write(work.path().join(name), bytes).unwrap();
        let handle = ws.file_handle(name).unwrap();
        for input in [name, handle.as_str()] {
            let out = run(&ws, args(json!({"path":input}))).unwrap().data.unwrap();
            assert_eq!(out["source"]["format"], format, "{input}");
            assert_eq!(out["source"]["path"], input);
            assert_eq!(out["groups"][0]["statistics"]["x"]["sum"], 7.0);
        }
    }
}

#[test]
fn csv_handles_quoted_multiline_escaped_fields_and_null_not_zero() {
    let bytes = b"name,amount,note\r\n\"A,B\",2,\"line1\nline2\"\r\n\"C\"\"D\",,\r\nE,4,x\r\n";
    let table = parse(bytes, "csv", None).unwrap();
    assert_eq!(table.rows[0].1["name"], "A,B");
    assert_eq!(table.rows[0].1["note"], "line1\nline2");
    assert_eq!(table.rows[1].1["name"], "C\"D");
    let out =
        query::calculate(&table, &args(json!({"path":"x.csv","columns":["amount"]}))).unwrap();
    let stat = &out["groups"][0]["statistics"]["amount"];
    assert_eq!(stat["count"], 3);
    assert_eq!(stat["null"], 1);
    assert_eq!(stat["mean"], 3.0);
    assert_eq!(stat["sum"], 6.0);
    assert_eq!(stat["distinct"], 2);
}

#[test]
fn filters_grouping_and_window_report_exact_used_rows() {
    let out = calculate(br#"[{"team":"a","x":1},{"team":"b","x":2},{"team":"a","x":3},{"team":"a","x":4}]"#,"json",json!({"path":"x.json","action":"aggregate","columns":["x"],"group_by":["team"],"filters":[{"column":"x","op":"ge","value":2}],"offset":1,"limit":1})).unwrap();
    assert_eq!(out["used_row_ids"], json!([3]));
    assert_eq!(out["coverage"]["source_rows"], 4);
    assert_eq!(out["coverage"]["matched_rows"], 3);
    assert_eq!(out["groups"][0]["key"], json!(["a"]));
    assert_eq!(out["groups"][0]["statistics"]["x"]["sum"], 3.0);
}

#[test]
fn missing_values_and_empty_selections_have_null_not_zero_statistics() {
    let input = br#"[{"x":null},{"other":2}]"#;
    let out = calculate(input, "json", json!({"path":"x","columns":["x"]})).unwrap();
    let stat = &out["groups"][0]["statistics"]["x"];
    assert_eq!(stat["null"], 2);
    assert!(stat["sum"].is_null());
    assert_eq!(stat["distinct"], 0);
    let empty = calculate(input, "json", json!({"path":"x","offset":20})).unwrap();
    assert_eq!(empty["coverage"]["used_rows"], 0);
    assert!(empty["groups"][0]["statistics"]["x"]["mean"].is_null());
}

#[test]
fn refuses_nested_data_unknown_columns_invalid_query_and_unsafe_numeric_values() {
    assert!(parse(br#"[{"a":{}}]"#, "json", None).is_err());
    assert!(parse(b"a,a\n1,2", "csv", None).is_err());
    assert!(parse(b"a,b\n1,2,3", "csv", None).is_err());
    assert!(
        calculate(
            br#"[{"a":1}]"#,
            "json",
            json!({"path":"x","columns":["unknown"]})
        )
        .is_err()
    );
    assert!(calculate(br#"[{"a":9007199254740993}]"#, "json", json!({"path":"x"})).is_err());
    assert!(calculate(br#"[{"a":1e999}]"#, "json", json!({"path":"x"})).is_err());
    assert!(
        calculate(
            br#"[{"a":1}]"#,
            "json",
            json!({"path":"x","filters":[{"column":"a","op":"eval","value":"evil"}]})
        )
        .is_err()
    );
    assert!(serde_json::from_value::<Args>(json!({"path":"x","sql":"drop table"})).is_err());
}

#[test]
fn mixed_columns_do_not_silently_sum_only_numeric_subset() {
    let out = calculate(br#"[{"x":2},{"x":"bad"}]"#, "json", json!({"path":"x"})).unwrap();
    assert!(out["groups"][0]["statistics"]["x"]["sum"].is_null());
    assert_eq!(out["groups"][0]["statistics"]["x"]["numeric_count"], 1);
    assert!(
        calculate(
            br#"[{"x":2},{"x":"bad"}]"#,
            "json",
            json!({"path":"x","action":"chart","columns":["x"]})
        )
        .is_err()
    );
}

#[test]
fn durable_calculation_and_chart_cite_original_identity_and_exact_query() {
    let (work, ws) = testkit::workspace();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(ArtifactStore::new(state.path()).unwrap());
    let original = b"label,x\n<script>,2\nother,-3\n";
    fs::write(work.path().join("x.csv"), original).unwrap();
    let out = run(&ws,args(json!({"path":"x.csv","action":"chart","columns":["x"],"group_by":["label"],"retain_source":true}))).unwrap();
    let data = out.data.unwrap();
    let store = ws.artifacts().unwrap();
    let citation = data["evidence_references"][0].as_str().unwrap();
    let record = store.evidence_record(citation).unwrap();
    assert_eq!(
        record.evidence.context.snapshot.as_ref().unwrap().sha256,
        super::super::evidence::source::snapshot(original)
            .unwrap()
            .sha256
    );
    assert!(
        record
            .evidence
            .context
            .snapshot
            .as_ref()
            .unwrap()
            .artifact
            .is_some()
    );
    let retained: Value =
        serde_json::from_slice(&store.evidence_bytes(&record, MAX_RESULT).unwrap()).unwrap();
    assert_eq!(retained["query"]["action"], "chart");
    assert_eq!(retained["used_row_ids"], json!([1, 2]));
    assert_eq!(retained["used_rows"][0]["values"]["label"], "<script>");
    assert_eq!(retained["used_rows"][0]["values"]["x"], 2);
    assert_eq!(retained["source"]["snapshot"]["bytes"], original.len());
    let chart = store
        .evidence_record(data["chart"]["evidence_references"][0].as_str().unwrap())
        .unwrap();
    assert_eq!(chart.evidence.context.parents, vec![citation]);
    let svg = String::from_utf8(store.evidence_bytes(&chart, MAX_RESULT).unwrap()).unwrap();
    assert!(!svg.contains("<script>"));
    assert!(svg.contains("&lt;script&gt;"));
    fs::write(work.path().join("x.csv"), b"changed").unwrap();
    assert_eq!(
        store.evidence_bytes(&record, MAX_RESULT).unwrap(),
        serde_json::to_vec_pretty(&retained).unwrap()
    );
}

#[test]
fn authorized_source_reader_rejects_outside_roots() {
    let (_work, ws) = testkit::workspace();
    assert!(run(&ws, args(json!({"path":"../outside.csv"}))).is_err());
}

#[test]
fn bounded_rows_and_results_never_silently_truncate() {
    let csv = format!("x\n{}", "1\n".repeat(MAX_ROWS + 1));
    assert!(parse(csv.as_bytes(), "csv", None).is_err());
    assert!(calculate(br#"[{"x":1}]"#, "json", json!({"path":"x","limit":1001})).is_err());
}

#[test]
fn xlsx_structured_values_preserve_actual_sheet_rows_and_partial_workbook_coverage() {
    use std::io::Write;
    let (work, ws) = testkit::workspace();
    let mut zip = zip::ZipWriter::new(fs::File::create(work.path().join("table.xlsx")).unwrap());
    for (name, text) in [
        (
            "xl/workbook.xml",
            r#"<workbook xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Data" r:id="r1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="r1" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetData><row r="2"><c r="A2" t="inlineStr"><is><t>amount</t></is></c></row><row r="5"><c r="A5"><v>4</v></c></row><row r="7"><c r="A7"><v>8</v></c></row></sheetData></worksheet>"#,
        ),
    ] {
        zip.start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(text.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
    let out = run(&ws, args(json!({"path":"table.xlsx","columns":["amount"]}))).unwrap();
    let data = out.data.unwrap();
    assert_eq!(data["source"]["sheet"], "Data");
    assert_eq!(data["used_row_ids"], json!([5, 7]));
    assert_eq!(data["used_rows"][0]["values"]["amount"], 4);
    assert_eq!(data["groups"][0]["statistics"]["amount"]["mean"], 6.0);
    assert_eq!(
        data["evidence"]["items"][0]["context"]["coverage"]["extent"],
        "partial"
    );
}

#[test]
fn lexical_numeric_boundaries_reject_rounded_decimal_and_exponent_in_stats_and_comparisons() {
    for literal in [
        "9007199254740993.0",
        "9007199254740993e0",
        "9.007199254740993e15",
        "-9007199254740993.0",
        "-9007199254740993e0",
    ] {
        for format in ["json", "csv"] {
            let input = if format == "json" {
                format!(r#"[{{"x":{literal}}}]"#)
            } else {
                format!("x\n{literal}\n")
            };
            let error = calculate(input.as_bytes(), format, json!({"path":"x"})).unwrap_err();
            assert!(
                format!("{error:#}").contains("exact f64 range"),
                "{format} {literal}: {error:#}"
            );
            for op in ["eq", "lt"] {
                let query = json!({"path":"x","action":"select","filters":[{"column":"x","op":op,"value":9007199254740992_u64}]});
                assert!(
                    calculate(input.as_bytes(), format, query).is_err(),
                    "source {format} {literal} {op}"
                );
                let rhs: Value = serde_json::from_str(literal).unwrap();
                let query = json!({"path":"x","action":"select","filters":[{"column":"x","op":op,"value":rhs}]});
                let safe = if format == "json" {
                    r#"[{"x":9007199254740992}]"#
                } else {
                    "x\n9007199254740992\n"
                };
                assert!(
                    calculate(safe.as_bytes(), format, query).is_err(),
                    "filter {format} {literal} {op}"
                );
            }
        }
    }
}

#[test]
fn exact_two_to_53_boundary_is_supported_in_all_lexical_forms() {
    for literal in [
        "9007199254740992",
        "9007199254740992.0",
        "9007199254740992e0",
        "9.007199254740992e15",
        "-9007199254740992",
        "-9007199254740992.0",
        "-9007199254740992e0",
    ] {
        for format in ["json", "csv"] {
            let input = if format == "json" {
                format!(r#"[{{"x":{literal}}}]"#)
            } else {
                format!("x\n{literal}\n")
            };
            let out = calculate(input.as_bytes(), format, json!({"path":"x"})).unwrap();
            let expected = if literal.starts_with('-') {
                -9007199254740992.0
            } else {
                9007199254740992.0
            };
            assert_eq!(
                out["groups"][0]["statistics"]["x"]["sum"],
                json!(expected),
                "{format} {literal}"
            );
            let rhs: Value = serde_json::from_str(literal).unwrap();
            let out = calculate(input.as_bytes(), format,json!({"path":"x","action":"select","filters":[{"column":"x","op":"eq","value":rhs}]})).unwrap();
            assert_eq!(out["coverage"]["used_rows"], 1, "{format} {literal}");
        }
    }
}

#[test]
fn cancelled_table_parsing_rejects_each_format_before_work() {
    let cancel = AtomicBool::new(true);
    for format in ["json", "csv", "xlsx"] {
        let error = table::parse(b"unparsed", format, None, &cancel)
            .err()
            .unwrap();
        assert!(error.to_string().contains("cancelled"));
    }
}

#[test]
fn streaming_data_tool_honors_cancel_before_opening_source() {
    let (_work, ws) = testkit::workspace();
    let config = Config::default();
    let ctx = ToolContext {
        workspace: &ws,
        config: &config,
        mutation_authorizer: None,
        confirm: None,
    };
    let cancel = AtomicBool::new(true);
    let out = DataTool.call_streaming(&ctx, &json!({"path":"missing.xlsx"}), &|_| {}, &cancel);
    assert!(out.is_error);
    assert!(
        out.text.contains("data operation cancelled"),
        "{}",
        out.text
    );
}

#[test]
fn histogram_quantiles_outliers_and_bins_link_exact_rows() {
    let out = calculate(
        b"x\n0\n1\n2\n3\n100\n\n",
        "csv",
        json!({"path":"x","action":"histogram","columns":["x"],"bins":2}),
    )
    .unwrap();
    let s = &out["investigation"];
    assert_eq!(s["quantiles"]["median"], 2.0);
    assert_eq!(s["outliers"]["row_ids"], json!([5]));
    assert_eq!(s["bins"][0]["row_ids"], json!([1, 2, 3, 4]));
    assert_eq!(s["bins"][1]["row_ids"], json!([5]));
    assert_eq!(s["bins"][1]["upper_inclusive"], true);
}

#[test]
fn paired_trend_discloses_missing_values_and_requires_order() {
    let input = br#"[{"t":3,"y":7},{"t":1,"y":3},{"t":2,"y":5},{"t":4,"y":null}]"#;
    assert!(
        calculate(
            input,
            "json",
            json!({"path":"x","action":"trend","columns":["y"]})
        )
        .is_err()
    );
    let out = calculate(
        input,
        "json",
        json!({"path":"x","action":"trend","columns":["y"],"order_by":"t"}),
    )
    .unwrap();
    let s = &out["investigation"];
    assert_eq!(s["points"][0]["row_ids"], json!([2]));
    assert_eq!(s["excluded_null_row_ids"], json!([4]));
    assert_eq!(s["fit"]["slope"], 2.0);
    assert_eq!(s["fit"]["intercept"], 1.0);
    assert!((s["fit"]["pearson_r"].as_f64().unwrap() - 1.0).abs() < 1e-12);
}

#[test]
fn constant_and_empty_columns_have_no_invented_correlation() {
    for input in [
        br#"[{"x":2,"y":3},{"x":2,"y":3}]"#.as_slice(),
        br#"[{"x":null,"y":null}]"#,
    ] {
        let out = calculate(
            input,
            "json",
            json!({"path":"x","action":"correlation","columns":["x","y"]}),
        )
        .unwrap();
        assert!(out["investigation"]["fit"]["pearson_r"].is_null());
        assert!(out["investigation"]["fit"]["slope"].is_null());
        let out = calculate(
            input,
            "json",
            json!({"path":"x","action":"histogram","columns":["x"]}),
        )
        .unwrap();
        assert!(out["investigation"]["bins"].as_array().unwrap().len() <= 1);
    }
}

#[test]
fn brush_and_row_selection_are_explicit_bounded_and_before_window() {
    let input = b"x\n1\n2\n3\n4\n";
    let out = calculate(input,"csv",json!({"path":"x","action":"select","row_ids":[2,3,4],"brush":{"column":"x","min":2,"max":3},"offset":1})).unwrap();
    assert_eq!(out["used_row_ids"], json!([3]));
    for extra in [
        json!({"row_ids":[9]}),
        json!({"row_ids":[1,1]}),
        json!({"brush":{"column":"x","min":3,"max":1}}),
        json!({"bins":101}),
    ] {
        let mut query = json!({"path":"x"});
        query
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(calculate(input, "csv", query).is_err());
    }
}

#[test]
fn retained_plot_drilldown_does_not_reopen_source_and_links_marks() {
    let (work, ws) = testkit::workspace();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(ArtifactStore::new(state.path()).unwrap());
    fs::write(work.path().join("x.csv"), b"x,y\n1,3\n2,5\n3,7\n").unwrap();
    let data = run(
        &ws,
        args(json!({"path":"x.csv","action":"correlation","columns":["x","y"]})),
    )
    .unwrap()
    .data
    .unwrap();
    let reference = data["evidence_references"][0].as_str().unwrap();
    let chart = ws
        .artifacts()
        .unwrap()
        .evidence_record(data["chart"]["evidence_references"][0].as_str().unwrap())
        .unwrap();
    let svg = String::from_utf8(
        ws.artifacts()
            .unwrap()
            .evidence_bytes(&chart, MAX_RESULT)
            .unwrap(),
    )
    .unwrap();
    assert!(svg.contains(&format!("href=\"{reference}\"")));
    assert!(svg.contains("data-row-ids=\"[2]\""));
    fs::remove_file(work.path().join("x.csv")).unwrap();
    let selected = run(&ws,args(json!({"path":"x.csv","calculation_reference":reference,"action":"select","row_ids":[2],"brush":{"column":"y","min":5,"max":5}}))).unwrap().data.unwrap();
    assert_eq!(selected["used_row_ids"], json!([2]));
    assert_eq!(selected["rows"][0]["values"]["y"], 5);
    assert_eq!(selected["parent_calculation"], reference);
}

#[test]
fn investigations_reuse_lexical_guards_and_reject_mixed_values() {
    for literal in [
        "9007199254740993.0",
        "9.007199254740993e15",
        "1e999",
        "\"text\"",
    ] {
        let input = format!(r#"[{{"x":{literal},"y":1}}]"#);
        for (action, columns) in [
            ("histogram", json!(["x"])),
            ("correlation", json!(["x", "y"])),
        ] {
            assert!(
                calculate(
                    input.as_bytes(),
                    "json",
                    json!({"path":"x","action":action,"columns":columns})
                )
                .is_err()
            );
        }
    }
    // Extreme but safe input magnitudes retain finite descriptive calculations.
    assert!(
        calculate(
            br#"[{"x":0,"y":0},{"x":1e-160,"y":9007199254740992}]"#,
            "json",
            json!({"path":"x","action":"correlation","columns":["x","y"]})
        )
        .is_ok()
    );
}

#[test]
fn histogram_refuses_collapsed_edges_and_retains_boundary_membership() {
    assert!(
        calculate(
            b"x\n9007199254740991\n9007199254740992\n",
            "csv",
            json!({"path":"x","action":"histogram","columns":["x"]})
        )
        .is_err()
    );
    let out = calculate(
        b"x\n0\n1\n2\n",
        "csv",
        json!({"path":"x","action":"histogram","columns":["x"],"bins":2}),
    )
    .unwrap();
    assert_eq!(out["investigation"]["bins"][0]["row_ids"], json!([1]));
    assert_eq!(out["investigation"]["bins"][1]["row_ids"], json!([2, 3]));
}

#[test]
fn plot_labels_are_escaped_and_group_bars_exclude_null_contributors() {
    let mut calculation = calculate(
        b"g,x\na,2\na,\n",
        "csv",
        json!({"path":"x","action":"chart","columns":["x"],"group_by":["g"]}),
    )
    .unwrap();
    calculation["calculation_reference"] = json!("evidence://example");
    let svg = query::chart(&calculation).unwrap();
    assert!(svg.contains("data-row-ids=\"[1]\""));
    let mut calculation = calculate(
        b"<script>\n1\n2\n",
        "csv",
        json!({"path":"x","action":"histogram","columns":["<script>"]}),
    )
    .unwrap();
    calculation["calculation_reference"] = json!("evidence://example");
    let svg = investigate::chart(&calculation).unwrap();
    assert!(!svg.contains("<script>"));
    assert!(svg.contains("&lt;script&gt;"));
}

#[test]
fn whole_source_is_explicit_and_retained_replay_supports_full_bounded_window() {
    let csv = format!("x\n{}", "1\n".repeat(101));
    assert_eq!(
        calculate(csv.as_bytes(), "csv", json!({"path":"x"})).unwrap()["coverage"]["used_rows"],
        100
    );
    let out = calculate(
        csv.as_bytes(),
        "csv",
        json!({"path":"x","whole_source":true}),
    )
    .unwrap();
    assert_eq!(out["coverage"]["used_rows"], 101);
    assert_eq!(investigate::replay(&out).unwrap().rows.len(), 101);
    for extra in [json!({"limit":100}), json!({"offset":1})] {
        let mut a = json!({"path":"x","whole_source":true});
        a.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(calculate(csv.as_bytes(), "csv", a).is_err());
    }
}

#[test]
fn json_pointer_paths_escape_keys_and_jsonl_preserves_physical_line_identity() {
    let source = b"\n{\"a/b\":{\"~x\":[4]},\"a\":{\"b\":5}}\n\n{\"a/b\":{\"~x\":[6]}}\n";
    let table =
        table::parse_with_paths(source, "jsonl", None, true, &AtomicBool::new(false)).unwrap();
    assert_eq!(
        table.rows.iter().map(|r| r.0).collect::<Vec<_>>(),
        vec![2, 4]
    );
    assert_eq!(table.rows[0].1["/a~1b/~0x/0"], 4);
    assert_eq!(table.rows[0].1["/a/b"], 5);
    assert!(
        table::parse_with_paths(source, "jsonl", None, false, &AtomicBool::new(false)).is_err()
    );
    assert!(
        table::parse_with_paths(
            b"{\"x\":1}\nBAD",
            "jsonl",
            None,
            false,
            &AtomicBool::new(false)
        )
        .is_err()
    );
    assert!(
        table::parse_with_paths(
            br#"[{"x":[]}]"#,
            "json",
            None,
            true,
            &AtomicBool::new(false)
        )
        .is_err()
    );
}

#[test]
fn exact_decimal_sums_preserve_cancellation_precision_and_reject_unbounded_exponents() {
    let out = calculate(
        br#"[{"x":"9007199254740993.01"},{"x":"-9007199254740993"},{"x":"0.09"},{"x":null}]"#,
        "json",
        json!({"path":"x","decimal_columns":["x"]}),
    )
    .unwrap();
    let stat = &out["groups"][0]["statistics"]["x"];
    assert_eq!(stat["exact_sum"], "0.1");
    assert_eq!(stat["decimal_count"], 3);
    assert_eq!(stat["null"], 1);
    assert!(stat.get("sum").is_none());
    let out = calculate(
        b"x\n0.1\n0.2\n",
        "csv",
        json!({"path":"x","decimal_columns":["x"]}),
    )
    .unwrap();
    assert_eq!(out["groups"][0]["statistics"]["x"]["exact_sum"], "0.3");
    for value in ["1e10000000", "NaN", " 1", "1e-101", "1..2"] {
        let input = serde_json::to_vec(&json!([{"x":value}])).unwrap();
        assert!(
            calculate(&input, "json", json!({"path":"x","decimal_columns":["x"]})).is_err(),
            "{value}"
        );
    }
    assert!(
        calculate(
            b"x\n1\n",
            "csv",
            json!({"path":"x","action":"chart","columns":["x"],"decimal_columns":["x"]})
        )
        .is_err()
    );
}

#[test]
fn compatible_units_preserve_original_and_reject_dimensions_and_output_collision() {
    let mut table = parse(b"x\n1000\n", "csv", None).unwrap();
    interpret::apply(
        &mut table,
        &args(
            json!({"path":"x","conversions":[{"column":"x","output":"km","from":"m","to":"km"}]}),
        ),
    )
    .unwrap();
    assert_eq!(table.rows[0].1["x"], 1000);
    assert_eq!(table.rows[0].1["km"], 1.0);
    for (from, to, output) in [("m", "kg", "new"), ("USD", "EUR", "new"), ("m", "km", "x")] {
        let mut table = parse(b"x\n1\n", "csv", None).unwrap();
        assert!(interpret::apply(&mut table, &args(json!({"path":"x","conversions":[{"column":"x","output":output,"from":from,"to":to}]}))).is_err());
    }
    let mut table = parse(b"x\n32\n", "csv", None).unwrap();
    interpret::apply(&mut table, &args(json!({"path":"x","conversions":[{"column":"x","output":"c","from":"degF","to":"degC"}]}))).unwrap();
    assert!(table.rows[0].1["c"].as_f64().unwrap().abs() < 1e-10);
}

#[test]
fn strict_dates_require_real_calendar_values_and_explicit_known_offsets() {
    for (text, format) in [
        ("2024-02-30", "date"),
        ("2024-2-01", "date"),
        ("2024-01-01T01:00:00", "rfc3339"),
        ("2024-01-01T01:00:00-00:00", "rfc3339"),
        ("2016-12-31T23:59:60Z", "rfc3339"),
        ("2024-01-01 America/New_York", "rfc3339"),
    ] {
        let input = serde_json::to_vec(&json!([{"x":text}])).unwrap();
        let mut table = parse(&input, "json", None).unwrap();
        assert!(
            interpret::apply(
                &mut table,
                &args(
                    json!({"path":"x","dates":[{"column":"x","output":"parsed","format":format}]})
                )
            )
            .is_err(),
            "{text}"
        );
    }
    let mut table = parse(
        br#"[{"x":"1970-01-01T01:00:00+01:00"},{"x":"1970-01-01T00:00:00Z"}]"#,
        "json",
        None,
    )
    .unwrap();
    interpret::apply(
        &mut table,
        &args(json!({"path":"x","dates":[{"column":"x","output":"seconds","format":"rfc3339"}]})),
    )
    .unwrap();
    assert_eq!(table.rows[0].1["seconds"], 0.0);
    assert_eq!(table.rows[1].1["seconds"], 0.0);
    let mut table = parse(br#"[{"x":"2024-02-29"}]"#, "json", None).unwrap();
    interpret::apply(
        &mut table,
        &args(json!({"path":"x","dates":[{"column":"x","output":"date","format":"date"}]})),
    )
    .unwrap();
    assert_eq!(table.rows[0].1["date"], "2024-02-29");
}

#[test]
fn dictionary_is_bounded_unverified_metadata_with_row_addressed_constraints() {
    let table = parse(br#"[{"x":1},{"x":-999},{"x":20},{"x":null}]"#, "json", None).unwrap();
    let dict = json!({"origin":"model_inferred","row_meaning":"one reading","columns":{"x":{"meaning":"temperature","unit":"degC","missing_values":[-999],"nullable":false,"min":0,"max":10}}});
    let out =
        interpret::dictionary(&table, &serde_json::from_value(dict.clone()).unwrap()).unwrap();
    assert_eq!(out["human_approval"], "not_authenticated");
    assert_eq!(out["declaration"]["origin"], "model_inferred");
    assert_eq!(
        out["observations"]["x"]["declared_missing_row_ids"],
        json!([2, 4])
    );
    assert_eq!(
        out["observations"]["x"]["constraint_violation_row_ids"],
        json!([2, 3, 4])
    );
    assert_eq!(table.rows[1].1["x"], -999);
    let mut forged = dict;
    forged["human_approved"] = json!(true);
    assert!(serde_json::from_value::<interpret::Dictionary>(forged).is_err());
}

#[test]
fn joins_report_all_duplicate_pairs_unmatched_and_both_revisions_without_truncation() {
    let (work, ws) = testkit::workspace();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(ArtifactStore::new(state.path()).unwrap());
    fs::write(work.path().join("left.csv"), b"id,x\na,1\na,2\nb,3\n").unwrap();
    fs::write(work.path().join("right.CSV"), b"id,y\na,4\na,5\nc,6\n").unwrap();
    let right = ws.file_handle("right.CSV").unwrap();
    let a = json!({"path":"left.csv","action":"select","retain_source":true,"join":{"path":right,"left_on":["id"],"right_on":["id"],"kind":"full"}});
    let out = run(&ws, args(a.clone())).unwrap().data.unwrap();
    assert_eq!(out["join"]["joined_rows"], 6);
    assert_eq!(out["join"]["right_source"]["path"], right);
    assert_eq!(out["join"]["right_source"]["format"], "csv");
    assert_eq!(
        out["join"]["duplicate_left_keys"][0]["row_ids"],
        json!([1, 2])
    );
    assert_eq!(
        out["join"]["duplicate_right_keys"][0]["row_ids"],
        json!([1, 2])
    );
    assert_eq!(out["join"]["unmatched_left_row_ids"], json!([3]));
    assert_eq!(out["join"]["unmatched_right_row_ids"], json!([3]));
    assert_eq!(
        out["join"]["row_pairs"][1],
        json!({"row":2,"left_row":1,"right_row":2})
    );
    assert_eq!(out["rows"][0]["values"]["left:x"], 1);
    assert_eq!(out["rows"][0]["values"]["right:y"], 4);
    assert!(out["source"]["snapshot"]["sha256"].is_string());
    assert!(out["join"]["right_source"]["snapshot"]["sha256"].is_string());
    assert!(out["join"]["right_source"]["snapshot"]["artifact"].is_string());
    let mut limited = a;
    limited["join"]["max_rows"] = json!(3);
    assert!(
        run(&ws, args(limited))
            .unwrap_err()
            .to_string()
            .contains("max_rows")
    );
}

#[test]
fn workbook_inspection_retains_formula_text_without_execution_or_cached_value_claims() {
    use std::io::Write;
    let (work, ws) = testkit::workspace();
    let mut zip = zip::ZipWriter::new(fs::File::create(work.path().join("formula.XLSX")).unwrap());
    for (name, text) in [
        (
            "xl/workbook.xml",
            r#"<workbook xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Data" state="hidden" r:id="r1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<Relationships><Relationship Id="r1" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<worksheet><sheetData><row r="1"><c r="A1"><f>WEBSERVICE("https://invalid.example")</f><v>123</v></c></row></sheetData></worksheet>"#,
        ),
    ] {
        zip.start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(text.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
    let handle = ws.file_handle("formula.XLSX").unwrap();
    let out = run(&ws, args(json!({"path":handle,"action":"inspect"})))
        .unwrap()
        .data
        .unwrap();
    assert_eq!(out["action"], "inspect");
    assert_eq!(out["source"]["path"], handle);
    assert_eq!(out["sheets"][0]["name"], "Data");
    assert_eq!(
        out["sheets"][0]["formulas"][0]["expression"],
        "WEBSERVICE(\"https://invalid.example\")"
    );
    assert_eq!(
        out["sheets"][0]["formulas"][0]["cached_value_verified"],
        false
    );
    assert!(
        run(
            &ws,
            args(json!({"path":handle,"action":"inspect","columns":["x"]}))
        )
        .is_err()
    );
}

#[test]
fn dictionary_identifiers_report_duplicate_and_missing_rows_without_certifying_keys() {
    let table = parse(br#"[{"id":"a"},{"id":"a"},{"id":null}]"#, "json", None).unwrap();
    let dictionary = serde_json::from_value(json!({"origin":"caller_declared","row_meaning":"record","columns":{"id":{"meaning":"record identifier","identifier":true}}})).unwrap();
    let out = interpret::dictionary(&table, &dictionary).unwrap();
    assert_eq!(
        out["observations"]["id"]["duplicate_value_row_id_groups"],
        json!([[1, 2]])
    );
    assert_eq!(
        out["observations"]["id"]["constraint_violation_row_ids"],
        json!([1, 2, 3])
    );
    assert_eq!(out["human_approval"], "not_authenticated");
}
