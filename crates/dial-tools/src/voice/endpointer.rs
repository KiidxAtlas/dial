//! Energy-based speech endpointer.
//!
//! Transcription backends decode a complete waveform at once. To transcribe while the user is
//! still talking, the continuous 16 kHz mono stream is split into speech segments at natural
//! pauses: each segment is transcribed as it finalizes. Segmentation is short-time-energy voice
//! activity detection with an adaptive noise floor, so it needs no model and is deterministic.

/// Tunable thresholds. All durations are milliseconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EndpointerConfig {
    pub sample_rate: u32,
    /// Short-time analysis frame size.
    pub frame_ms: u32,
    /// Trailing silence inside a segment that finalizes it.
    pub end_silence_ms: u32,
    /// Shortest speech run that is committed; shorter runs are discarded as noise.
    pub min_speech_ms: u32,
    /// Hard cap on segment length so long pause-free speech still commits periodically.
    pub max_segment_ms: u32,
    /// Audio retained before onset so the first phoneme is never clipped.
    pub pre_roll_ms: u32,
    /// Speech threshold is `max(min_threshold, noise_floor * energy_ratio)`.
    pub energy_ratio: f32,
    /// EMA weight tracking the ambient noise floor on non-speech frames.
    pub floor_attack: f32,
    /// Absolute RMS floor so a near-silent room never trips speech detection.
    pub min_threshold: f32,
    /// Opening audio used to measure the room before any speech is detected; it is replayed
    /// afterwards, so speech in it is kept. Without it, a room louder than
    /// `min_threshold * energy_ratio` reads as endless speech: the floor only adapts on
    /// non-speech frames, so it would never learn that level.
    pub calibrate_ms: u32,
    /// Highest noise floor calibration may set, so speech from the first moment still counts.
    pub max_calibrated_floor: f32,
}

impl Default for EndpointerConfig {
    fn default() -> Self {
        Self {
            sample_rate: 16_000,
            frame_ms: 30,
            end_silence_ms: 600,
            min_speech_ms: 200,
            max_segment_ms: 12_000,
            pre_roll_ms: 240,
            energy_ratio: 2.5,
            floor_attack: 0.05,
            min_threshold: 0.008,
            calibrate_ms: 300,
            max_calibrated_floor: 0.04,
        }
    }
}

/// A finalized run of speech: transcribe and commit once.
#[derive(Debug, Clone, PartialEq)]
pub enum EndpointerEvent {
    Segment(Vec<f32>),
}

/// Fixed-capacity ring keeping the most recent `capacity` samples.
struct Ring {
    data: Vec<f32>,
    start: usize,
    len: usize,
}

impl Ring {
    fn new(capacity: usize) -> Self {
        Self {
            data: vec![0.0; capacity],
            start: 0,
            len: 0,
        }
    }

    fn push(&mut self, samples: &[f32]) {
        let cap = self.data.len();
        if cap == 0 {
            return;
        }
        if samples.len() >= cap {
            self.data.copy_from_slice(&samples[samples.len() - cap..]);
            self.start = 0;
            self.len = cap;
            return;
        }
        let pos = (self.start + self.len) % cap;
        let first = samples.len().min(cap - pos);
        self.data[pos..pos + first].copy_from_slice(&samples[..first]);
        if first < samples.len() {
            self.data[..samples.len() - first].copy_from_slice(&samples[first..]);
        }
        let total = self.len + samples.len();
        if total > cap {
            self.start = (self.start + total - cap) % cap;
            self.len = cap;
        } else {
            self.len = total;
        }
    }

    /// Append the retained samples, oldest first, to `target`.
    fn copy_into(&self, target: &mut Vec<f32>) {
        let cap = self.data.len();
        let end = self.start + self.len;
        target.extend_from_slice(&self.data[self.start..end.min(cap)]);
        if end > cap {
            target.extend_from_slice(&self.data[..end - cap]);
        }
    }

    fn reset(&mut self) {
        self.start = 0;
        self.len = 0;
    }
}

pub struct Endpointer {
    cfg: EndpointerConfig,
    frame_samples: usize,
    /// Partial frame carried to the next push; always shorter than one frame.
    leftover: Vec<f32>,
    in_speech: bool,
    noise_floor: f32,
    silence_ms: u32,
    /// Frames in the current segment that were above the threshold.
    voiced_ms: u32,
    /// Opening frames held for calibration; `None` once the floor is measured.
    calibration: Option<Vec<Vec<f32>>>,
    segment_ms: f64,
    /// Pre-roll included at the start of the current segment; not counted as speech.
    onset_ms: f64,
    segment: Vec<f32>,
    pre_roll: Ring,
}

impl Endpointer {
    pub fn new(cfg: EndpointerConfig) -> Self {
        let frame_samples = ((cfg.sample_rate as u64 * cfg.frame_ms as u64) / 1000).max(1) as usize;
        let pre_roll_samples = (cfg.sample_rate as u64 * cfg.pre_roll_ms as u64 / 1000) as usize;
        Self {
            cfg,
            frame_samples,
            leftover: Vec::with_capacity(frame_samples),
            in_speech: false,
            voiced_ms: 0,
            calibration: Some(Vec::new()),
            noise_floor: cfg.min_threshold,
            silence_ms: 0,
            segment_ms: 0.0,
            onset_ms: 0.0,
            segment: Vec::new(),
            pre_roll: Ring::new(pre_roll_samples),
        }
    }

    /// Whether a segment is in progress (speech was detected and has not yet ended).
    pub fn in_speech(&self) -> bool {
        self.in_speech
    }

    /// Feed newly captured samples; returns ordered partial/segment events.
    pub fn push(&mut self, samples: &[f32]) -> Vec<EndpointerEvent> {
        let mut events = Vec::new();
        let frame = self.frame_samples;
        let mut offset = 0;
        if !self.leftover.is_empty() {
            let fill = (frame - self.leftover.len()).min(samples.len());
            self.leftover.extend_from_slice(&samples[..fill]);
            offset = fill;
            if self.leftover.len() < frame {
                return events;
            }
            let carried = std::mem::take(&mut self.leftover);
            self.process_frame(&carried, &mut events);
            self.leftover = carried;
            self.leftover.clear();
        }
        while offset + frame <= samples.len() {
            self.process_frame(&samples[offset..offset + frame], &mut events);
            offset += frame;
        }
        self.leftover.extend_from_slice(&samples[offset..]);
        events
    }

    /// End the stream; returns a trailing committed segment if one is pending.
    pub fn flush(&mut self) -> Vec<EndpointerEvent> {
        let mut events = Vec::new();
        self.finish_calibration(&mut events);
        if self.in_speech && !self.leftover.is_empty() {
            self.segment.extend_from_slice(&self.leftover);
            self.segment_ms += self.leftover.len() as f64 * 1000.0 / self.cfg.sample_rate as f64;
        }
        self.leftover.clear();
        if self.in_speech && self.committable() {
            let keep = self.endpoint_keep();
            events.push(EndpointerEvent::Segment(self.segment[..keep].to_vec()));
        }
        self.reset();
        events
    }

    fn process_frame(&mut self, frame: &[f32], events: &mut Vec<EndpointerEvent>) {
        if let Some(held) = &mut self.calibration {
            held.push(frame.to_vec());
            if held.len() as u32 * self.cfg.frame_ms >= self.cfg.calibrate_ms {
                self.finish_calibration(events);
            }
            return;
        }
        let energy = super::wav::rms(frame);
        let threshold = self
            .cfg
            .min_threshold
            .max(self.noise_floor * self.cfg.energy_ratio);
        let voiced = energy > threshold;
        // Track ambient noise on non-speech frames only, so loud speech never inflates the floor
        // (which would make the tail of an utterance read as silence and clip it short).
        if !voiced {
            self.noise_floor =
                self.noise_floor * (1.0 - self.cfg.floor_attack) + energy * self.cfg.floor_attack;
        }
        if !self.in_speech {
            if voiced {
                self.begin_segment(frame);
            } else {
                self.pre_roll.push(frame);
            }
            return;
        }
        self.segment.extend_from_slice(frame);
        self.segment_ms += self.cfg.frame_ms as f64;
        if voiced {
            self.silence_ms = 0;
            self.voiced_ms += self.cfg.frame_ms;
        } else {
            self.silence_ms += self.cfg.frame_ms;
        }
        if self.silence_ms >= self.cfg.end_silence_ms {
            self.finalize_segment(events);
            return;
        }
        if self.segment_ms >= self.cfg.max_segment_ms as f64 {
            // Pause-free long speech: commit what we have and continue a fresh segment.
            events.push(EndpointerEvent::Segment(std::mem::take(&mut self.segment)));
            self.segment_ms = 0.0;
            self.onset_ms = 0.0;
            self.silence_ms = 0;
            self.voiced_ms = 0;
        }
    }

    fn begin_segment(&mut self, onset: &[f32]) {
        self.in_speech = true;
        self.segment.clear();
        self.pre_roll.copy_into(&mut self.segment);
        self.onset_ms = self.segment.len() as f64 * 1000.0 / self.cfg.sample_rate as f64;
        self.segment.extend_from_slice(onset);
        self.pre_roll.reset();
        self.silence_ms = 0;
        self.voiced_ms = self.cfg.frame_ms;
        self.segment_ms = self.segment.len() as f64 * 1000.0 / self.cfg.sample_rate as f64;
    }

    /// Measure the room from the held opening frames (their quietest frame, capped), then
    /// process them as usual.
    fn finish_calibration(&mut self, events: &mut Vec<EndpointerEvent>) {
        let Some(held) = self.calibration.take() else {
            return;
        };
        let quietest = held
            .iter()
            .map(|frame| super::wav::rms(frame))
            .fold(f32::INFINITY, f32::min);
        if quietest.is_finite() {
            self.noise_floor = quietest
                .min(self.cfg.max_calibrated_floor)
                .max(self.cfg.min_threshold);
        }
        for frame in &held {
            self.process_frame(frame, events);
        }
    }

    /// Voiced duration of the current segment: pre-roll and trailing silence excluded, so a click
    /// after a quiet stretch is not mistaken for speech.
    fn speech_ms(&self) -> f64 {
        self.segment_ms - self.silence_ms as f64 - self.onset_ms
    }

    /// Long enough to be speech, counting both its span and the frames actually voiced, so
    /// noise that only flickers over the threshold is not committed.
    fn committable(&self) -> bool {
        let min = self.cfg.min_speech_ms;
        self.speech_ms() >= f64::from(min) && self.voiced_ms >= min
    }

    fn finalize_segment(&mut self, events: &mut Vec<EndpointerEvent>) {
        if self.committable() {
            let keep = self.endpoint_keep();
            self.segment.truncate(keep);
            events.push(EndpointerEvent::Segment(std::mem::take(&mut self.segment)));
        }
        self.in_speech = false;
        self.segment.clear();
        self.silence_ms = 0;
        self.voiced_ms = 0;
        self.segment_ms = 0.0;
        self.onset_ms = 0.0;
    }

    /// Samples to keep when committing on silence: drop most trailing silence but leave a short
    /// tail so the final word is not cut.
    fn endpoint_keep(&self) -> usize {
        let tail_ms = self.silence_ms.min(120);
        let drop_ms = self.silence_ms.saturating_sub(tail_ms);
        let drop = (self.cfg.sample_rate as u64 * drop_ms as u64 / 1000) as usize;
        self.segment.len().saturating_sub(drop)
    }

    fn reset(&mut self) {
        self.in_speech = false;
        self.segment.clear();
        self.pre_roll.reset();
        self.silence_ms = 0;
        self.voiced_ms = 0;
        self.calibration = Some(Vec::new());
        self.segment_ms = 0.0;
        self.onset_ms = 0.0;
        self.noise_floor = self.cfg.min_threshold;
    }
}

#[cfg(test)]
mod tests;
