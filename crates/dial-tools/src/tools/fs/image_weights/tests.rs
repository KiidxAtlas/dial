use super::*;
use crate::tools::integrations::ext::Registry;
use crate::tools::{Workspace, testkit};
use safetensors::tensor::TensorView;
use std::collections::HashMap;

fn fixture() -> Vec<u8> {
    let floats = [1.5f32, -2.0]
        .into_iter()
        .flat_map(f32::to_le_bytes)
        .collect::<Vec<_>>();
    let integer = 7u32.to_le_bytes();
    let tensors = [
        (
            "layer.weight",
            TensorView::new(Dtype::F32, vec![2], &floats).unwrap(),
        ),
        (
            "untouched",
            TensorView::new(Dtype::U32, vec![1], &integer).unwrap(),
        ),
    ];
    safetensors::tensor::serialize(
        tensors,
        Some(HashMap::from([("format".into(), "pt".into())])),
    )
    .unwrap()
}
fn config() -> Config {
    let mut config = Config::default();
    config.image.allow_weight_edits = true;
    config
}
fn run(ws: &Workspace, config: &Config, args: Value) -> ToolOutput {
    let authorize = testkit::authorize_in_workspace(ws, TOOL);
    ImageWeights.call(
        &ToolContext {
            workspace: ws,
            config,
            mutation_authorizer: Some(&authorize),
            confirm: None,
        },
        &args,
    )
}
fn scale_args_for(bytes: &[u8]) -> Value {
    json!({"action":"scale", "source":"source.safetensors", "output":"scaled.safetensors",
        "sha256":digest(bytes), "scales":{"layer.weight":-2.0}})
}

#[test]
fn scales_only_selected_values_preserving_source_header_and_other_tensors() {
    let (_dir, ws) = testkit::workspace();
    let original = fixture();
    std::fs::write(ws.root().join("source.safetensors"), &original).unwrap();
    let out = run(&ws, &config(), scale_args_for(&original));
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(
        std::fs::read(ws.root().join("source.safetensors")).unwrap(),
        original
    );
    let saved = std::fs::read(ws.root().join("scaled.safetensors")).unwrap();
    let before = SafeTensors::deserialize(&original).unwrap();
    let after = SafeTensors::deserialize(&saved).unwrap();
    let actual = after.tensor("layer.weight").unwrap();
    assert_eq!(actual.dtype(), Dtype::F32);
    assert_eq!(actual.shape(), &[2]);
    assert_eq!(
        actual.data(),
        [-3.0f32, 4.0]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        after.tensor("untouched").unwrap().data(),
        before.tensor("untouched").unwrap().data()
    );
    // Everything outside the selected tensor's data range, including metadata, is byte-identical.
    let start = actual.data().as_ptr() as usize - saved.as_ptr() as usize;
    assert_eq!(&saved[..start], &original[..start]);
    assert_eq!(
        &saved[start + actual.data().len()..],
        &original[start + actual.data().len()..]
    );
    let report: Value = serde_json::from_str(&out.text).unwrap();
    assert_eq!(report["sha256"], digest(&saved));
    assert_eq!(std::fs::read_dir(ws.root()).unwrap().count(), 2);
}

#[test]
fn inspection_is_paged_filtered_and_does_not_create_files() {
    let (_dir, ws) = testkit::workspace();
    let bytes = fixture();
    std::fs::write(ws.root().join("source.safetensors"), &bytes).unwrap();
    for (extra, expected, next) in [
        (json!({"limit":1}), vec!["layer.weight"], Some(1)),
        (json!({"limit":1,"offset":1}), vec!["untouched"], None),
        (json!({"prefix":"layer."}), vec!["layer.weight"], None),
        (json!({"offset":500}), vec![], None),
    ] {
        let mut args = json!({"action":"inspect","source":"source.safetensors"});
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let out = run(&ws, &config(), args);
        assert!(!out.is_error, "{}", out.text);
        let data: Value = serde_json::from_str(&out.text).unwrap();
        let names = data["tensors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, expected);
        assert_eq!(data["next_offset"], json!(next));
        assert_eq!(data["sha256"], digest(&bytes));
    }
    assert_eq!(std::fs::read_dir(ws.root()).unwrap().count(), 1);
}

#[test]
fn floating_encodings_scale_and_refuse_overflow_nonfinite_and_cancellation() {
    let cancel = AtomicBool::new(false);
    for dtype in [Dtype::F16, Dtype::BF16, Dtype::F32, Dtype::F64] {
        let (mut bytes, expected, mut overflow) = match dtype {
            Dtype::F16 => (
                half::f16::from_f32(1.5).to_bits().to_le_bytes().to_vec(),
                half::f16::from_f32(-3.0).to_bits().to_le_bytes().to_vec(),
                half::f16::MAX.to_bits().to_le_bytes().to_vec(),
            ),
            Dtype::BF16 => (
                half::bf16::from_f32(1.5).to_bits().to_le_bytes().to_vec(),
                half::bf16::from_f32(-3.0).to_bits().to_le_bytes().to_vec(),
                half::bf16::MAX.to_bits().to_le_bytes().to_vec(),
            ),
            Dtype::F32 => (
                1.5f32.to_le_bytes().to_vec(),
                (-3.0f32).to_le_bytes().to_vec(),
                f32::MAX.to_le_bytes().to_vec(),
            ),
            Dtype::F64 => (
                1.5f64.to_le_bytes().to_vec(),
                (-3.0f64).to_le_bytes().to_vec(),
                f64::MAX.to_le_bytes().to_vec(),
            ),
            _ => unreachable!(),
        };
        scale_values(&mut bytes, dtype, -2.0, &cancel).unwrap();
        assert_eq!(bytes, expected, "{dtype:?}");
        assert!(
            scale_values(&mut overflow, dtype, 8.0, &cancel).is_err(),
            "{dtype:?}"
        );
    }
    assert!(scale_values(&mut f32::NAN.to_le_bytes(), Dtype::F32, 1.0, &cancel).is_err());
    assert!(
        scale_values(
            &mut 1f32.to_le_bytes(),
            Dtype::F32,
            1.0,
            &AtomicBool::new(true)
        )
        .is_err()
    );
}

#[test]
fn refusal_boundaries_publish_nothing_and_keep_existing_files() {
    let (_dir, ws) = testkit::workspace();
    let bytes = fixture();
    std::fs::write(ws.root().join("source.safetensors"), &bytes).unwrap();
    std::fs::write(ws.root().join("existing.safetensors"), b"keep").unwrap();
    std::fs::write(ws.root().join("bad.safetensors"), b"not a checkpoint").unwrap();
    for (field, value) in [
        ("scales", json!({"missing":1})),
        ("scales", json!({"untouched":1})),
        ("scales", json!({"layer.weight":9})),
        ("scales", json!({})),
        ("sha256", json!("0".repeat(64))),
        ("output", json!("existing.safetensors")),
        ("output", json!("source.safetensors")),
        ("output", json!("../escape.safetensors")),
        ("output", json!("missing/out.safetensors")),
        ("source", json!("../escape.safetensors")),
        ("source", json!("bad.safetensors")),
    ] {
        let mut args = scale_args_for(&bytes);
        args[field] = value;
        let out = run(&ws, &config(), args);
        assert!(out.is_error, "{field}: {}", out.text);
        assert!(!ws.root().join("scaled.safetensors").exists());
        assert_eq!(std::fs::read_dir(ws.root()).unwrap().count(), 3);
    }
    assert_eq!(
        std::fs::read(ws.root().join("existing.safetensors")).unwrap(),
        b"keep"
    );
    assert_eq!(
        std::fs::read(ws.root().join("source.safetensors")).unwrap(),
        bytes
    );
}

#[test]
fn setting_authority_and_approval_are_independent_gates() {
    let (_dir, ws) = testkit::workspace();
    let bytes = fixture();
    std::fs::write(ws.root().join("source.safetensors"), &bytes).unwrap();
    let args = scale_args_for(&bytes);
    let disabled = run(&ws, &Config::default(), args.clone());
    assert!(
        disabled.is_error && disabled.text.contains("disabled"),
        "{}",
        disabled.text
    );
    let cfg = config();
    let out = ImageWeights.call(
        &ToolContext {
            workspace: &ws,
            config: &cfg,
            mutation_authorizer: None,
            confirm: None,
        },
        &args,
    );
    assert!(
        out.is_error && out.text.contains("authorization"),
        "{}",
        out.text
    );
    assert!(ImageWeights.needs_approval(&args));
    assert!(Registry::standard(&cfg, vec![]).contains(TOOL));
    assert_eq!(std::fs::read_dir(ws.root()).unwrap().count(), 1);
}

#[test]
fn staged_publication_refuses_racing_writes_and_cancellation_and_cleans_up() {
    use crate::tools::edit::edit_transaction::NewBinary;
    let (_dir, ws) = testkit::workspace();
    let output = ws.root().join("output.safetensors");
    let mut staged = NewBinary::create(&ws, &output).unwrap();
    staged.file_mut().write_all(b"scaled").unwrap();
    std::fs::write(&output, b"racing writer").unwrap();
    assert!(staged.publish(&ws, &AtomicBool::new(false)).is_err());
    assert_eq!(std::fs::read(&output).unwrap(), b"racing writer");
    assert_eq!(std::fs::read_dir(ws.root()).unwrap().count(), 1);
    let cancelled = ws.root().join("cancelled.safetensors");
    let mut staged = NewBinary::create(&ws, &cancelled).unwrap();
    staged.file_mut().write_all(b"scaled").unwrap();
    assert!(staged.publish(&ws, &AtomicBool::new(true)).is_err());
    assert!(!cancelled.exists());
    assert_eq!(std::fs::read_dir(ws.root()).unwrap().count(), 1);
}

#[cfg(unix)]
#[test]
fn replaced_output_parent_cannot_redirect_publication() {
    use crate::tools::edit::edit_transaction::NewBinary;
    let (_dir, ws) = testkit::workspace();
    let outside = tempfile::tempdir().unwrap();
    let parent = ws.root().join("export");
    std::fs::create_dir(&parent).unwrap();
    let mut staged = NewBinary::create(&ws, &parent.join("output.safetensors")).unwrap();
    staged.file_mut().write_all(b"scaled").unwrap();
    let moved = outside.path().join("moved");
    std::fs::rename(&parent, &moved).unwrap();
    std::os::unix::fs::symlink(&moved, &parent).unwrap();
    assert!(staged.publish(&ws, &AtomicBool::new(false)).is_err());
    assert_eq!(std::fs::read_dir(&moved).unwrap().count(), 0);
}

#[test]
fn cancellation_after_authorization_publishes_nothing() {
    let (_dir, ws) = testkit::workspace();
    let bytes = fixture();
    std::fs::write(ws.root().join("source.safetensors"), &bytes).unwrap();
    let cancel = AtomicBool::new(false);
    let authorize = testkit::authorize_in_workspace(&ws, TOOL);
    let cancel_on_authorization = |tool: &str, paths: &[std::path::PathBuf], process: bool| {
        let result = authorize(tool, paths, process);
        cancel.store(true, Ordering::SeqCst);
        result
    };
    let cfg = config();
    let out = ImageWeights.call_streaming(
        &ToolContext {
            workspace: &ws,
            config: &cfg,
            mutation_authorizer: Some(&cancel_on_authorization),
            confirm: None,
        },
        &scale_args_for(&bytes),
        &|_| {},
        &cancel,
    );
    assert!(
        out.is_error && out.text.contains("interrupted"),
        "{}",
        out.text
    );
    assert_eq!(std::fs::read_dir(ws.root()).unwrap().count(), 1);
}
