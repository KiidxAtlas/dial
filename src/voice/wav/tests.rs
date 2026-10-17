use super::*;

#[test]
fn wav_header_describes_mono_pcm16() {
    let wav = encode_pcm16(&[0.0, 1.0, -1.0, 2.0], 16_000);
    assert_eq!(&wav[..4], b"RIFF");
    assert_eq!(&wav[8..16], b"WAVEfmt ");
    assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16_000);
    assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 8);
    assert_eq!(wav.len(), 52);
    assert_eq!(i16::from_le_bytes([wav[46], wav[47]]), 32767);
    assert_eq!(i16::from_le_bytes([wav[48], wav[49]]), -32768);
    assert_eq!(
        i16::from_le_bytes([wav[50], wav[51]]),
        32767,
        "out of range clamps"
    );
}

#[test]
fn pcm_decoding_returns_odd_trailing_byte() {
    let mut out = Vec::new();
    let rest = pcm16le_into(&[0x00, 0x40, 0x00, 0xC0, 0x7F], &mut out);
    assert_eq!(out, vec![0.5, -0.5]);
    assert_eq!(rest, Some(0x7F));
}

#[test]
fn downmix_averages_frames() {
    let mut out = Vec::new();
    downmix_into(&[1.0, 0.0, 0.5, 0.5], 2, &mut out);
    assert_eq!(out, vec![0.5, 0.5]);
}

#[test]
fn resampling_is_chunk_independent_and_scales_length() {
    let input: Vec<f32> = (0..4800).map(|i| (i as f32 * 0.01).sin()).collect();
    let mut whole = Vec::new();
    Resampler::new(48_000, 16_000).process(&input, &mut whole);
    let mut chunked = Vec::new();
    let mut resampler = Resampler::new(48_000, 16_000);
    for chunk in input.chunks(37) {
        resampler.process(chunk, &mut chunked);
    }
    assert_eq!(whole.len(), chunked.len());
    assert!((whole.len() as i64 - 1600).abs() <= 1, "{}", whole.len());
    for (a, b) in whole.iter().zip(&chunked) {
        assert!((a - b).abs() < 1e-5);
    }
    let mut up = Vec::new();
    Resampler::new(8_000, 16_000).process(&input[..800], &mut up);
    assert!((up.len() as i64 - 1600).abs() <= 2, "{}", up.len());
}
