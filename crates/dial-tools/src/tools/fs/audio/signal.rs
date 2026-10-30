//! Deterministic energy measurements on bounded, channel-preserving decoded PCM.
use super::*;
use std::sync::atomic::Ordering;

const RATE: usize = 16_000;
const MAX_CHANNELS: usize = 8;
const MAX_WINDOWS: u64 = 256;

#[derive(Clone, Copy)]
pub(super) struct Params {
    window_ms: u64,
    silence_dbfs: f64,
    clip_threshold: f64,
    transient_db: f64,
}
impl Params {
    pub(super) fn from_args(a: &Args) -> Result<Self> {
        let supplied = a.window_ms.is_some()
            || a.silence_dbfs.is_some()
            || a.clip_threshold.is_some()
            || a.transient_db.is_some();
        ensure!(
            a.action == "analyze" || !supplied,
            "signal parameters require action analyze"
        );
        let p = Self {
            window_ms: a.window_ms.unwrap_or(1000),
            silence_dbfs: a.silence_dbfs.unwrap_or(-50.0),
            clip_threshold: a.clip_threshold.unwrap_or(0.999),
            transient_db: a.transient_db.unwrap_or(12.0),
        };
        ensure!(
            (100..=10_000).contains(&p.window_ms),
            "window_ms must be 100..10000"
        );
        ensure!(
            p.silence_dbfs.is_finite() && (-120.0..=0.0).contains(&p.silence_dbfs),
            "silence_dbfs must be -120..0"
        );
        ensure!(
            p.clip_threshold.is_finite() && p.clip_threshold > 0.0 && p.clip_threshold <= 1.0,
            "clip_threshold must be >0..1"
        );
        ensure!(
            p.transient_db.is_finite() && (0.0..=120.0).contains(&p.transient_db),
            "transient_db must be 0..120"
        );
        if a.action == "analyze" {
            ensure!(
                (a.end_ms - a.start_ms).div_ceil(p.window_ms) <= MAX_WINDOWS,
                "at most 256 analysis windows; increase window_ms or shorten interval"
            );
        }
        Ok(p)
    }
}

pub(super) fn run(
    ctx: &ToolContext,
    a: &Args,
    p: Params,
    bytes: &[u8],
    mut item: Evidence,
    cancel: &AtomicBool,
) -> Result<ToolOutput> {
    let converter = ffmpeg()?;
    let dir = tempfile::tempdir()?;
    let input = dir.path().join("source");
    std::fs::write(&input, bytes)?;
    let spec = CommandSpec::new(converter)
        .arg("-nostdin")
        .arg("-v")
        .arg("error")
        .arg("-format_whitelist")
        .arg("mov,matroska,webm,avi,wav,mp3,flac,ogg,aac,aiff")
        .arg("-protocol_whitelist")
        .arg("file,pipe")
        .arg("-i")
        .arg(&input)
        .arg("-ss")
        .arg(format!("{:.3}", a.start_ms as f64 / 1000.0))
        .arg("-t")
        .arg(format!("{:.3}", (a.end_ms - a.start_ms) as f64 / 1000.0))
        .arg("-map")
        .arg("0:a:0")
        .arg("-vn")
        .arg("-ar")
        .arg(RATE.to_string())
        .arg("-c:a")
        .arg("pcm_s16le")
        .arg("-f")
        .arg("wav")
        .arg("pipe:1");
    let wav = process::run(&spec, None, cancel, Duration::from_secs(60))?.stdout;
    let (channels, pcm) = parse_wav(&wav)?;
    let frames = pcm.len() / (2 * channels);
    ensure!(
        frames > 0 && frames as u64 <= (a.end_ms - a.start_ms) * 16,
        "empty or oversized decoded interval"
    );
    let end = a.start_ms as f64 + frames as f64 / 16.0;
    let mut data = measure(pcm, channels, a.start_ms, p, cancel)?;
    data["source_interval_ms"] = json!([a.start_ms as f64, end]);
    data["requested_interval_ms"] = json!([a.start_ms, a.end_ms]);
    data["omitted_tail_ms"] = if end < a.end_ms as f64 {
        json!([end, a.end_ms as f64])
    } else {
        Value::Null
    };
    item.method = Method::Calculation;
    item.producer = "dial/audio-signal-ffmpeg-pcm16-v1".into();
    item.limitations = vec!["Signal measurements are on resampled 16 kHz signed PCM16, not original encoded samples; threshold clipping is not proof of source overload. Only the first audio stream is decoded; channels are preserved in decoder order (no speaker labels).".into(), "Energy events and transients are threshold heuristics with window-sized boundary uncertainty, not sound classification, speech detection, identity or diarization. Independent clocks are not aligned by parent links. Outside the selected interval is uninspected. Integer evidence endpoints round outward by less than 1 ms; JSON retains the exact decoded end.".into()];
    // Evidence locations use integer milliseconds; the JSON retains the exact fractional decoded end.
    let end_ms = end.ceil() as u64;
    item.location = Location::Time {
        start_ms: a.start_ms,
        end_ms,
    };
    let coverage = item
        .context
        .coverage
        .as_mut()
        .context("audio coverage missing")?;
    coverage.inspected = vec![item.location.clone()];
    if end_ms < a.end_ms {
        coverage.omitted = vec![Location::Time {
            start_ms: end_ms,
            end_ms: a.end_ms,
        }];
    }
    let text = serde_json::to_string_pretty(&data)?;
    ensure!(
        text.len() <= 2 * 1024 * 1024,
        "signal report exceeds 2 MiB; shorten interval or increase window_ms"
    );
    item.representation = Representation::of_bytes("application/json", text.as_bytes());
    let mut out = ToolOutput::ok(text.clone());
    out.data = Some(data);
    publish(ctx.workspace, out, item, text.as_bytes())
}

fn parse_wav(wav: &[u8]) -> Result<(usize, &[u8])> {
    ensure!(
        wav.len() >= 12 && &wav[..4] == b"RIFF" && &wav[8..12] == b"WAVE",
        "converter did not return RIFF WAV"
    );
    let mut offset = 12usize;
    let mut channels = None;
    while offset + 8 <= wav.len() {
        let id = &wav[offset..offset + 4];
        let size = u32::from_le_bytes(wav[offset + 4..offset + 8].try_into()?) as usize;
        offset += 8;
        if id == b"data" {
            let channels = channels.context("WAV data precedes format")?;
            // Streaming ffmpeg writes an unknown length sentinel; finite chunks must be complete.
            ensure!(
                size == u32::MAX as usize || size <= wav.len() - offset,
                "truncated WAV data"
            );
            let data = &wav[offset..offset + size.min(wav.len() - offset)];
            ensure!(
                data.len() <= RATE * 120 * MAX_CHANNELS * 2
                    && data.len().is_multiple_of(channels * 2),
                "oversized or incomplete PCM frames"
            );
            return Ok((channels, data));
        }
        ensure!(size <= wav.len() - offset, "truncated WAV chunk");
        if id == b"fmt " {
            let fmt = &wav[offset..offset + size];
            ensure!(fmt.len() >= 16, "short WAV format");
            let tag = u16::from_le_bytes(fmt[0..2].try_into()?);
            let count = u16::from_le_bytes(fmt[2..4].try_into()?) as usize;
            let rate = u32::from_le_bytes(fmt[4..8].try_into()?);
            let bits = u16::from_le_bytes(fmt[14..16].try_into()?);
            ensure!(
                tag == 1
                    || (tag == 0xfffe
                        && fmt.len() >= 40
                        && fmt[24..40]
                            == [1, 0, 0, 0, 0, 0, 16, 0, 128, 0, 0, 170, 0, 56, 155, 113]),
                "unsupported WAV sample encoding"
            );
            ensure!(
                (1..=MAX_CHANNELS).contains(&count) && rate == RATE as u32 && bits == 16,
                "analysis supports 1..8 preserved PCM16 channels at 16 kHz"
            );
            channels = Some(count);
        }
        offset = offset
            .checked_add(size)
            .and_then(|next| next.checked_add(size % 2))
            .context("WAV chunk overflow")?;
    }
    anyhow::bail!("WAV has no PCM data")
}

fn db(amplitude: f64) -> Option<f64> {
    (amplitude > 0.0).then(|| 20.0 * amplitude.log10())
}
fn sample(pcm: &[u8], frame: usize, channel: usize, channels: usize) -> f64 {
    let pos = (frame * channels + channel) * 2;
    f64::from(i16::from_le_bytes([pcm[pos], pcm[pos + 1]])) / 32768.0
}
fn measure(
    pcm: &[u8],
    channels: usize,
    start_ms: u64,
    p: Params,
    cancel: &AtomicBool,
) -> Result<Value> {
    let frames = pcm.len() / (channels * 2);
    let width = p.window_ms as usize * RATE / 1000;
    let silence = 10.0_f64.powf(p.silence_dbfs / 20.0);
    let mut reports = Vec::new();
    let mut comparisons = Vec::new();
    for ch in 0..channels {
        let mut windows = Vec::new();
        let mut events = Vec::new();
        let mut event_start = None;
        let mut total_energy = 0.0;
        let mut peak = 0.0_f64;
        let mut clipped = 0usize;
        let mut silent_frames = 0usize;
        let mut previous = 0.0_f64;
        let mut cross = 0.0;
        let mut reference_energy = 0.0;
        for first in (0..frames).step_by(width) {
            ensure!(!cancel.load(Ordering::Relaxed), "audio analysis cancelled");
            let last = (first + width).min(frames);
            let mut energy = 0.0;
            let mut win_peak = 0.0_f64;
            let mut win_clipped = 0usize;
            for frame in first..last {
                let v = sample(pcm, frame, ch, channels);
                let reference = sample(pcm, frame, 0, channels);
                energy += v * v;
                cross += reference * v;
                reference_energy += reference * reference;
                win_peak = win_peak.max(v.abs());
                win_clipped += usize::from(v.abs() >= p.clip_threshold);
            }
            let rms = (energy / (last - first) as f64).sqrt();
            let silent = rms <= silence;
            if silent {
                silent_frames += last - first;
            }
            let begin = start_ms as f64 + first as f64 / 16.0;
            let end = start_ms as f64 + last as f64 / 16.0;
            if !silent && event_start.is_none() {
                event_start = Some(begin);
            }
            if silent && let Some(begin_event) = event_start.take() {
                events.push(json!([begin_event, begin]));
            }
            let rise_db = 20.0 * (rms.max(silence) / previous.max(silence)).log10();
            windows.push(json!({"interval_ms":[begin,end],"peak":win_peak,"rms":rms,"rms_dbfs":db(rms),"clipped_samples":win_clipped,"silent":silent,"transient_rise":!silent && rise_db >= p.transient_db,"rise_db_above_floor":rise_db}));
            previous = rms;
            total_energy += energy;
            peak = peak.max(win_peak);
            clipped += win_clipped;
        }
        if let Some(begin) = event_start {
            events.push(json!([begin, start_ms as f64 + frames as f64 / 16.0]));
        }
        let rms = (total_energy / frames as f64).sqrt();
        reports.push(json!({"channel_index":ch,"peak":peak,"peak_dbfs":db(peak),"rms":rms,"rms_dbfs":db(rms),"clipped_samples":clipped,"clipped_fraction":clipped as f64/frames as f64,"silent_window_fraction":silent_frames as f64/frames as f64,"energy_event_intervals_ms":events,"windows":windows}));
        if ch > 0 {
            let denom = (total_energy * reference_energy).sqrt();
            comparisons.push(json!({"channel_index":ch,"reference_channel_index":0,"normalized_uncentered_correlation":if denom>0.0 {Some(cross/denom)} else {None},"rms_difference_from_reference":rms-(reference_energy/frames as f64).sqrt()}));
        }
    }
    Ok(
        json!({"parameters":{"window_ms":p.window_ms,"silence_dbfs":p.silence_dbfs,"clip_threshold":p.clip_threshold,"transient_db":p.transient_db,"sample_rate_hz":RATE,"sample_encoding":"signed PCM16 little endian / 32768","audio_stream_index":0,"channel_policy":"preserved decoder order","silence_rule":"window RMS <= threshold","clipping_rule":"absolute decoded sample >= threshold","transient_rule":"window RMS rise versus preceding window (first uses silence floor), both floored at silence threshold","event_rule":"contiguous windows above silence threshold","zero_dbfs":"null denotes negative infinity (zero amplitude)"},"decoded_frames":frames,"channels":reports,"channel_comparisons":comparisons,"interpretation":"signal measurements only; no semantic sound labels, speech or speaker identity"}),
    )
}

#[cfg(test)]
mod tests;
