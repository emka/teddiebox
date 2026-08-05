//! Generates deterministic .taf fixtures for the TAF parser tests.
//!
//! Host-only. Never compiled for the device.

use std::f32::consts::PI;
use std::fs::File;
use std::path::{Path, PathBuf};

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u32 = 2;
const TONE_HZ: f32 = 440.0;
const SECONDS: u32 = 5;
const CHAPTER_SECONDS: u32 = 2;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "sine.taf".to_string());
    let path = PathBuf::from(path);

    write_single_chapter(&path)?;
    write_multi_chapter(&path.with_file_name("chapters.taf"))?;

    Ok(())
}

/// The original single-chapter fixture: a plain 5 s tone, one chapter.
/// Behaviour is unchanged from before this file gained a second fixture --
/// still deterministic, still byte-for-byte reproducible.
fn write_single_chapter(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let samples = sine_interleaved(SECONDS * SAMPLE_RATE);

    // `new`, not `new_simple`: new_simple generates a *random* audio id, and
    // the characterisation tests assert a known one.
    let file = File::create(path)?;
    let mut taf = toniefile::Toniefile::new(file, 0x1234_5678, None)?;
    taf.encode(&samples)?;
    taf.finalize()?;

    println!(
        "wrote {}: {} frames",
        path.display(),
        samples.len() / CHANNELS as usize
    );
    Ok(())
}

/// A three-chapter fixture, ~2 s of tone per chapter, via `new_chapter()`
/// between `encode()` calls. `sine.taf` has exactly one chapter, so it can't
/// exercise `TafReader::seek_to_chapter` beyond the single trivial case; this
/// fixture is what the reader's chapter tests actually seek across.
fn write_multi_chapter(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let chapter_samples = sine_interleaved(CHAPTER_SECONDS * SAMPLE_RATE);

    let file = File::create(path)?;
    let mut taf = toniefile::Toniefile::new(file, 0x1234_5678, None)?;
    taf.encode(&chapter_samples)?; // chapter 0, created implicitly by `new`
    taf.new_chapter()?;
    taf.encode(&chapter_samples)?; // chapter 1
    taf.new_chapter()?;
    taf.encode(&chapter_samples)?; // chapter 2
    taf.finalize()?;

    println!("wrote {}: 3 chapters", path.display());
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
