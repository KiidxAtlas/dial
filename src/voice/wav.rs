//! PCM helpers: 16-bit WAV encoding, raw PCM decoding, downmixing and streaming resampling.

/// Sample rate every capture backend delivers and every transcriber receives.
pub const SAMPLE_RATE: u32 = 16_000;

/// Convert one float sample (`[-1, 1]`, clamped) to signed 16-bit.
pub fn to_i16(sample: f32) -> i16 {
    let clamped = if sample.is_finite() {
        sample.clamp(-1.0, 1.0)
    } else {
        0.0
    };
    if clamped < 0.0 {
        (clamped * 32768.0) as i16
    } else {
        (clamped * 32767.0) as i16
    }
}

/// Encode mono float samples as a 16-bit little-endian PCM WAV file.
pub fn encode_pcm16(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    let data_bytes = (samples.len() * 2) as u32;
    let mut wav = Vec::with_capacity(44 + samples.len() * 2);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_bytes.to_le_bytes());
    for &sample in samples {
        wav.extend_from_slice(&to_i16(sample).to_le_bytes());
    }
    wav
}

/// Decode signed 16-bit little-endian PCM into float samples, appending to `out`. A trailing odd
/// byte is returned so the caller can prepend it to the next chunk.
pub fn pcm16le_into(bytes: &[u8], out: &mut Vec<f32>) -> Option<u8> {
    let (pairs, rest) = bytes.as_chunks::<2>();
    out.extend(
        pairs
            .iter()
            .map(|&pair| i16::from_le_bytes(pair) as f32 / 32768.0),
    );
    rest.first().copied()
}

/// Average interleaved frames of `channels` samples into mono, appending to `out`.
pub fn downmix_into(interleaved: &[f32], channels: usize, out: &mut Vec<f32>) {
    if channels <= 1 {
        out.extend_from_slice(interleaved);
        return;
    }
    let scale = 1.0 / channels as f32;
    out.extend(
        interleaved
            .chunks_exact(channels)
            .map(|frame| frame.iter().sum::<f32>() * scale),
    );
}

/// Linear-interpolating streaming resampler. Chunk boundaries do not change the output.
pub struct Resampler {
    step: f64,
    pos: f64,
    prev: Option<f32>,
    scratch: Vec<f32>,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        Self {
            step: from.max(1) as f64 / to.max(1) as f64,
            pos: 0.0,
            prev: None,
            scratch: Vec::new(),
        }
    }

    pub fn is_identity(&self) -> bool {
        self.step == 1.0
    }

    /// Resample `input`, appending output samples to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.is_identity() {
            out.extend_from_slice(input);
            return;
        }
        self.scratch.clear();
        self.scratch.extend(self.prev);
        self.scratch.extend_from_slice(input);
        let buf = &self.scratch;
        if buf.len() < 2 {
            self.prev = buf.last().copied();
            return;
        }
        while self.pos + 1.0 < buf.len() as f64 {
            let index = self.pos as usize;
            let frac = (self.pos - index as f64) as f32;
            out.push(buf[index] + (buf[index + 1] - buf[index]) * frac);
            self.pos += self.step;
        }
        // The next call's buffer starts at this buffer's last sample.
        self.pos -= (buf.len() - 1) as f64;
        self.prev = buf.last().copied();
    }
}

/// Root-mean-square level of `samples` (0 for an empty slice).
pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

#[cfg(test)]
mod tests;
