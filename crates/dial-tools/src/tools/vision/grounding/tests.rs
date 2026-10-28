use super::*;

#[test]
fn thousandths_reply_maps_to_screenshot_pixels() {
    // UI-Venus-1.5-2B answered `[761, 780]` for the Save button centered at (980, 625) of 1280x800.
    let (x, y) = parse_point("[761, 780]", Coordinates::Thousandths, 1280, 800).unwrap();
    assert!(
        (x - 974.08).abs() < 0.01 && (y - 624.0).abs() < 0.01,
        "{x},{y}"
    );
}

#[test]
fn smart_resize_reply_maps_back_from_the_processor_size() {
    // UI-TARS-1.5-7B answered `(993,634)` for the same button; the processor saw 1288x812.
    assert_eq!(smart_resize(1280, 800, 28, 3136, 12_845_056), (1288, 812));
    let (x, y) = parse_point(
        "<|box_start|>(993,634)<|box_end|>",
        QWEN2_VL_RESIZE,
        1280,
        800,
    )
    .unwrap();
    assert!(
        (x - 986.8).abs() < 0.1 && (y - 624.6).abs() < 0.1,
        "{x},{y}"
    );
}

#[test]
fn smart_resize_bounds_the_pixel_count() {
    let (w, h) = smart_resize(8000, 6000, 28, 3136, 1_000_000);
    assert!(u64::from(w) * u64::from(h) <= 1_000_000);
    assert_eq!((w % 28, h % 28), (0, 0));
    let (w, h) = smart_resize(10, 10, 28, 3136, 1_000_000);
    assert!(u64::from(w) * u64::from(h) >= 3136);
}

#[test]
fn unit_and_auto_replies() {
    assert_eq!(
        parse_point("[0.5, 0.25]", Coordinates::Unit, 200, 100).unwrap(),
        (100.0, 25.0)
    );
    assert_eq!(
        parse_point("(0.5, 0.25)", Coordinates::Auto, 200, 100).unwrap(),
        (100.0, 25.0)
    );
    assert_eq!(
        parse_point("x=500 y=250", Coordinates::Auto, 200, 100).unwrap(),
        (100.0, 25.0)
    );
}

#[test]
fn a_box_reply_yields_its_center() {
    assert_eq!(
        parse_point("[100, 200, 300, 400]", Coordinates::Thousandths, 1000, 1000).unwrap(),
        (200.0, 300.0)
    );
}

#[test]
fn replies_without_a_point_or_outside_the_image_are_refused() {
    assert!(parse_point("I cannot see it", Coordinates::Thousandths, 100, 100).is_err());
    assert!(parse_point("[1200, 10]", Coordinates::Thousandths, 100, 100).is_err());
    assert!(parse_point("[-5, 10]", Coordinates::Unit, 100, 100).is_err());
}

#[test]
fn coordinates_follow_local_config_then_name() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.json"),
        r#"{"model_type":"qwen2_5_vl"}"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("preprocessor_config.json"),
        r#"{"patch_size":14,"merge_size":2,"min_pixels":3136,"max_pixels":2000000}"#,
    )
    .unwrap();
    assert_eq!(
        coordinates_for("anything", Some(dir.path())),
        Coordinates::SmartResize {
            factor: 28,
            min_pixels: 3136,
            max_pixels: 2_000_000
        }
    );
    std::fs::write(
        dir.path().join("config.json"),
        r#"{"model_type":"qwen3_vl"}"#,
    )
    .unwrap();
    assert_eq!(
        coordinates_for("anything", Some(dir.path())),
        Coordinates::Thousandths
    );
    assert_eq!(
        coordinates_for("bytedance/ui-tars-1.5-7b", None),
        QWEN2_VL_RESIZE
    );
    assert_eq!(
        coordinates_for("mlx-community/UI-Venus-1.5-2B-6bit", None),
        Coordinates::Thousandths
    );
    assert_eq!(
        coordinates_for("showlab/ShowUI-2B", None),
        Coordinates::Unit
    );
    assert_eq!(coordinates_for("gpt-4o", None), Coordinates::Auto);
}

#[test]
fn each_family_gets_its_trained_prompt() {
    assert!(
        prompt(QWEN2_VL_RESIZE, "Save").contains("What element matches the following task: Save")
    );
    assert!(prompt(Coordinates::Thousandths, "Save").contains("in the format [x,y]"));
    assert!(prompt(Coordinates::Unit, "Save").contains("scaled from 0 to 1"));
}

#[test]
#[ignore = "loads a downloaded grounding model; set DIAL_TEST_GROUNDING_MODEL, DIAL_TEST_GROUNDING_IMAGE, DIAL_TEST_GROUNDING_TARGET (description) and DIAL_TEST_GROUNDING_EXPECT (x,y)"]
fn locates_a_described_element_with_the_real_model() {
    let env = |key: &str| std::env::var(key).unwrap_or_else(|_| panic!("{key} is required"));
    let screenshot = std::fs::read(env("DIAL_TEST_GROUNDING_IMAGE")).unwrap();
    let (want_x, want_y) = env("DIAL_TEST_GROUNDING_EXPECT")
        .split_once(',')
        .map(|(x, y)| {
            (
                x.trim().parse::<i64>().unwrap(),
                y.trim().parse::<i64>().unwrap(),
            )
        })
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let home = std::env::var("HOME").unwrap();
    let mut config = Config {
        user_dir: root.path().join("user"),
        state_dir: root.path().join("state"),
        project_root: root.path().to_path_buf(),
        python: std::path::PathBuf::from(home).join(".dial/python/.venv/bin/python"),
        ..Default::default()
    };
    config.model_roles.insert(
        ROLE.into(),
        crate::llm::model_config::ModelSelection::new("mlx", &env("DIAL_TEST_GROUNDING_MODEL")),
    );
    let located = locate(
        &config,
        &screenshot,
        &env("DIAL_TEST_GROUNDING_TARGET"),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    release();
    println!("LOCATED: {located:?}");
    let (x, y) = (i64::from(located.x), i64::from(located.y));
    assert!(
        (x - want_x).abs() <= 40 && (y - want_y).abs() <= 40,
        "{x},{y} is not near {want_x},{want_y}"
    );
}
