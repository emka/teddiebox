//! Decodes the fixtures and checks the result is what they were encoded from:
//! 440 Hz on the left, 660 Hz on the right.
//!
//! The tolerances are wide, so subtly wrong audio would still pass. Checking
//! the device's output exactly needs reference PCM, not these tests.

// The helpers below are test code too, but outside a `#[test]` function,
// where `allow-unwrap-in-tests` does not reach. A failed unwrap here is a
// failed test, which is what the lint is not meant to prevent.
#![allow(clippy::unwrap_used)]

use teddiebox_audio::{LibOpus, OpusState, TafDecoder, CHANNELS, MAX_FRAME_SAMPLES, SAMPLE_RATE};
use teddiebox_taf::SlicePages;

const FIXTURE: &[u8] = include_bytes!("../../teddiebox-taf/tests/data/sine.taf");
const CHAPTERS: &[u8] = include_bytes!("../../teddiebox-taf/tests/data/chapters.taf");

fn decode_all() -> Vec<i16> {
    decode_from(FIXTURE, None)
}

/// Decodes `taf` to the end, optionally seeking to a chapter first.
fn decode_from(taf: &[u8], chapter: Option<usize>) -> Vec<i16> {
    let mut state = OpusState::new();
    let mut dec = TafDecoder::open(
        SlicePages::new(taf).unwrap(),
        LibOpus::new(&mut state).unwrap(),
    )
    .unwrap();
    if let Some(n) = chapter {
        dec.seek_to_chapter(n).unwrap();
    }
    let mut pcm = [0i16; MAX_FRAME_SAMPLES];
    let mut out = Vec::new();
    while let Some(n) = dec.next_frame(&mut pcm).unwrap() {
        out.extend_from_slice(&pcm[..n]);
    }
    out
}

/// Zero crossings on one channel over one second, starting `skip_seconds`
/// in. A sine of `f` Hz crosses zero `2f` times per second.
fn crossings_over_one_second(pcm: &[i16], channel: usize, skip_seconds: usize) -> usize {
    let samples: Vec<i16> = pcm
        .as_chunks::<CHANNELS>()
        .0
        .iter()
        .skip(skip_seconds * SAMPLE_RATE as usize)
        .take(SAMPLE_RATE as usize)
        .map(|f| f[channel])
        .collect();

    samples
        .windows(2)
        .filter(|w| (w[0] < 0) != (w[1] < 0))
        .count()
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
    // Skip the encoder's warm-up before measuring.
    let crossings = crossings_over_one_second(&decode_all(), 0, 1);
    assert!(
        (860..=900).contains(&crossings),
        "expected ~880 zero crossings for 440 Hz, got {crossings}"
    );
}

/// The channels carry different tones, so this catches a channel swap, a
/// mono downmix, or an interleaving mistake.
#[test]
fn the_left_and_right_channels_carry_their_own_tones() {
    let pcm = decode_all();

    let left = crossings_over_one_second(&pcm, 0, 1);
    let right = crossings_over_one_second(&pcm, 1, 1);

    assert!(
        (860..=900).contains(&left),
        "expected ~880 zero crossings for 440 Hz on the left, got {left}"
    );
    assert!(
        (1300..=1340).contains(&right),
        "expected ~1320 zero crossings for 660 Hz on the right, got {right}"
    );
}

/// Seeks with the real decoder and checks that audio comes out.
#[test]
fn seeking_to_a_chapter_yields_audible_audio_from_that_chapter_onward() {
    let whole = decode_from(CHAPTERS, None);
    let from_second = decode_from(CHAPTERS, Some(1));

    let peak = from_second.iter().map(|s| s.unsigned_abs()).max().unwrap();
    assert!(peak > 1000, "audio after the seek is silent, peak {peak}");

    let crossings = crossings_over_one_second(&from_second, 0, 0);
    assert!(
        (860..=900).contains(&crossings),
        "expected ~880 zero crossings for 440 Hz after the seek, got {crossings}"
    );

    // Chapter 1 of 3 starts about a third in, so seeking there must drop
    // about a third of the audio. Only checking that it is *shorter* would
    // also pass if the seek went to the end.
    let dropped = whole.len() as f64 - from_second.len() as f64;
    let fraction = dropped / whole.len() as f64;
    assert!(
        (0.25..=0.42).contains(&fraction),
        "seeking to chapter 1 of 3 should skip about a third of the audio, skipped {:.0}%",
        fraction * 100.0
    );
}

#[test]
fn the_decoded_signal_is_not_silence() {
    let pcm = decode_all();
    let peak = pcm.iter().map(|s| s.unsigned_abs()).max().unwrap();
    assert!(peak > 1000, "decoded audio is silent, peak {peak}");
}
