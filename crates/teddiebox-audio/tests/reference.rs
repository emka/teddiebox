//! Decodes the fixture and checks the result really is the tone that
//! `fixturegen` encoded. This is the same assertion Phase B step 9 runs on
//! device, so a device regression is comparable against a known-good host run.

use teddiebox_audio::{LibOpus, OpusState, TafDecoder, CHANNELS, MAX_FRAME_SAMPLES, SAMPLE_RATE};
use teddiebox_taf::SlicePages;

const FIXTURE: &[u8] = include_bytes!("../../teddiebox-taf/tests/data/sine.taf");

fn decode_all() -> Vec<i16> {
    let mut state = OpusState::new();
    let mut dec = TafDecoder::open(
        SlicePages::new(FIXTURE).unwrap(),
        LibOpus::new(&mut state).unwrap(),
    )
    .unwrap();
    let mut pcm = [0i16; MAX_FRAME_SAMPLES];
    let mut out = Vec::new();
    while let Some(n) = dec.next_frame(&mut pcm).unwrap() {
        out.extend_from_slice(&pcm[..n]);
    }
    out
}

#[test]
fn decodes_approximately_five_seconds_of_stereo_audio() {
    let pcm = decode_all();
    let frames = pcm.len() / CHANNELS;
    let seconds = frames as f64 / SAMPLE_RATE as f64;
    assert!(
        (4.9..=5.2).contains(&seconds),
        "expected ~5 s, got {seconds:.2} s"
    );
}

#[test]
fn the_decoded_signal_is_a_440_hz_tone() {
    let pcm = decode_all();
    // Skip the encoder's warm-up, then count zero crossings on the left
    // channel over one second. A 440 Hz sine crosses zero 880 times.
    let left: Vec<i16> = pcm
        .as_chunks::<CHANNELS>()
        .0
        .iter()
        .skip(SAMPLE_RATE as usize)
        .take(SAMPLE_RATE as usize)
        .map(|f| f[0])
        .collect();

    let crossings = left.windows(2).filter(|w| (w[0] < 0) != (w[1] < 0)).count();

    assert!(
        (860..=900).contains(&crossings),
        "expected ~880 zero crossings for 440 Hz, got {crossings}"
    );
}

#[test]
fn the_decoded_signal_is_not_silence() {
    let pcm = decode_all();
    let peak = pcm.iter().map(|s| s.unsigned_abs()).max().unwrap();
    assert!(peak > 1000, "decoded audio is silent, peak {peak}");
}
