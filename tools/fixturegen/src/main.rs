//! Generates a deterministic .taf fixture for the TAF parser tests.
//!
//! Host-only. Never compiled for the device.

use std::f32::consts::PI;
use std::fs::File;

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u32 = 2;
const TONE_HZ: f32 = 440.0;
const SECONDS: u32 = 5;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "sine.taf".to_string());

    let samples = sine_interleaved(SECONDS * SAMPLE_RATE);

    // `new`, not `new_simple`: new_simple generates a *random* audio id, and
    // the characterisation tests assert a known one.
    let file = File::create(&path)?;
    let mut taf = toniefile::Toniefile::new(file, 0x1234_5678, None)?;
    taf.encode(&samples)?;
    taf.finalize()?;

    println!("wrote {path}: {} frames", samples.len() / CHANNELS as usize);
    Ok(())
}

/// A 440 Hz tone, interleaved stereo, identical in both channels.
/// Deterministic so the fixture is reproducible byte-for-byte.
fn sine_interleaved(frames: u32) -> Vec<i16> {
    let mut out = Vec::with_capacity((frames * CHANNELS) as usize);
    for n in 0..frames {
        let t = n as f32 / SAMPLE_RATE as f32;
        let v = (2.0 * PI * TONE_HZ * t).sin();
        let s = (v * i16::MAX as f32 * 0.5) as i16;
        for _ in 0..CHANNELS {
            out.push(s);
        }
    }
    out
}
