use super::*;
use crate::storage::artifact::ArtifactStore;
use crate::tools::testkit;

fn args() -> Args {
    serde_json::from_value(
        json!({"action":"analyze","path":"a.wav","start_ms":250,"end_ms":2250,"window_ms":500}),
    )
    .unwrap()
}
fn stereo() -> Vec<u8> {
    let mut pcm = Vec::new();
    for frame in 0..RATE * 2 {
        let left: i16 = if (RATE / 2..RATE).contains(&frame) {
            16384
        } else {
            0
        };
        let right: i16 = -left;
        pcm.extend(left.to_le_bytes());
        pcm.extend(right.to_le_bytes());
    }
    pcm
}
fn wav(pcm: &[u8], channels: u16) -> Vec<u8> {
    let mut wav = b"RIFF".to_vec();
    wav.extend((pcm.len() as u32 + 36).to_le_bytes());
    wav.extend(b"WAVEfmt ");
    wav.extend(16u32.to_le_bytes());
    wav.extend(1u16.to_le_bytes());
    wav.extend(channels.to_le_bytes());
    wav.extend((RATE as u32).to_le_bytes());
    wav.extend((RATE as u32 * channels as u32 * 2).to_le_bytes());
    wav.extend((channels * 2).to_le_bytes());
    wav.extend(16u16.to_le_bytes());
    wav.extend(b"data");
    wav.extend((pcm.len() as u32).to_le_bytes());
    wav.extend(pcm);
    wav
}

#[test]
fn synthetic_pcm_preserves_channels_energy_events_and_comparison() {
    let p = Params::from_args(&args()).unwrap();
    let data = measure(&stereo(), 2, 250, p, &AtomicBool::new(false)).unwrap();
    assert_eq!(data["channels"][0]["peak"], 0.5);
    assert_eq!(data["channels"][0]["rms"], 0.25);
    assert_eq!(data["channels"][0]["silent_window_fraction"], 0.75);
    assert_eq!(
        data["channels"][0]["energy_event_intervals_ms"],
        json!([[750.0, 1250.0]])
    );
    assert_eq!(data["channels"][0]["windows"][1]["transient_rise"], true);
    assert_eq!(
        data["channel_comparisons"][0]["normalized_uncentered_correlation"],
        -1.0
    );
    assert_eq!(data["channels"][0]["clipped_samples"], 0);
}

#[test]
fn clipping_silence_zero_and_cancel_are_explicit() {
    let p = Params::from_args(&args()).unwrap();
    let mut pcm = Vec::new();
    for _ in 0..1600 {
        pcm.extend(i16::MIN.to_le_bytes());
    }
    let data = measure(&pcm, 1, 0, p, &AtomicBool::new(false)).unwrap();
    assert_eq!(data["channels"][0]["clipped_fraction"], 1.0);
    let zeros = vec![0; 3200];
    let data = measure(&zeros, 1, 0, p, &AtomicBool::new(false)).unwrap();
    assert!(data["channels"][0]["rms_dbfs"].is_null());
    assert_eq!(data["channels"][0]["silent_window_fraction"], 1.0);
    assert!(measure(&zeros, 1, 0, p, &AtomicBool::new(true)).is_err());
}

#[test]
fn parameters_and_wav_headers_are_bounded() {
    let mut a = args();
    a.window_ms = Some(99);
    assert!(Params::from_args(&a).is_err());
    a.window_ms = Some(100);
    a.end_ms = 120_000;
    assert!(Params::from_args(&a).is_err());
    a = args();
    a.clip_threshold = Some(0.0);
    assert!(Params::from_args(&a).is_err());
    a = args();
    a.action = "replay".into();
    assert!(Params::from_args(&a).is_err());
    assert!(parse_wav(b"untrusted").is_err());
    let source = wav(&stereo(), 2);
    let (channels, pcm) = parse_wav(&source).unwrap();
    assert_eq!(channels, 2);
    assert_eq!(pcm, stereo());
    assert!(parse_wav(&wav(&[0; 4], 9)).is_err());
    assert!(parse_wav(&wav(&[0; 3], 2)).is_err());
}

#[test]
fn installed_ffmpeg_analysis_retains_citations_channels_and_omitted_tail() {
    if ffmpeg().is_err() {
        eprintln!("optional installed ffmpeg analysis tier unavailable");
        return;
    }
    let (dir, ws) = testkit::workspace();
    let state = tempfile::tempdir().unwrap();
    let ws = ws.with_artifacts(ArtifactStore::new(state.path()).unwrap());
    std::fs::write(dir.path().join("a.wav"), wav(&stereo(), 2)).unwrap();
    let config = Config::default();
    let ctx = ToolContext {
        workspace: &ws,
        config: &config,
        confirm: Some(&|_| true),
        mutation_authorizer: Some(&|_, _, _| Ok(())),
    };
    let out = super::super::execute(
        &ctx,
        &json!({"action":"analyze","path":"a.wav","start_ms":500,"end_ms":2500,"window_ms":500,"retain_source":true}),
        &AtomicBool::new(false),
    );
    assert!(!out.is_error, "{}", out.text);
    let data = out.data.as_ref().unwrap();
    assert_eq!(data["channels"].as_array().unwrap().len(), 2);
    assert_eq!(data["source_interval_ms"], json!([500.0, 2000.0]));
    assert_eq!(data["omitted_tail_ms"], json!([2000.0, 2500.0]));
    assert_eq!(data["channels"][0]["peak"], 0.5);
    assert_eq!(
        data["channel_comparisons"][0]["normalized_uncentered_correlation"],
        -1.0
    );
    assert!(
        data["evidence_references"][0]
            .as_str()
            .unwrap()
            .starts_with("artifact://")
    );
    let evidence = out.evidence().unwrap().unwrap();
    assert_eq!(evidence.items[0].method, Method::Calculation);
    assert_eq!(
        evidence.items[0].context.coverage.as_ref().unwrap().omitted,
        vec![Location::Time {
            start_ms: 2000,
            end_ms: 2500
        }]
    );
    let parent = data["evidence_references"][0].as_str().unwrap();
    let linked = super::super::execute(
        &ctx,
        &json!({"action":"analyze","path":"a.wav","start_ms":500,"end_ms":1000,"parents":[parent]}),
        &AtomicBool::new(false),
    );
    assert!(!linked.is_error, "{}", linked.text);
    assert_eq!(
        linked.evidence().unwrap().unwrap().items[0].context.parents,
        vec![parent.to_owned()]
    );
    let cancelled = super::super::execute(
        &ctx,
        &json!({"action":"analyze","path":"a.wav","start_ms":0,"end_ms":500}),
        &AtomicBool::new(true),
    );
    assert!(cancelled.is_error);
}
