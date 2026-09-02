//! Decodes a .taf to a .wav. The M1 demonstration, and the tool used to
//! listen to anything the device later claims it cannot play.

use std::fs;

use teddiebox_audio::{LibOpus, OpusState, TafDecoder, CHANNELS, MAX_FRAME_SAMPLES, SAMPLE_RATE};
use teddiebox_taf::SlicePages;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let (Some(input), Some(output)) = (args.next(), args.next()) else {
        eprintln!("usage: taf2wav <input.taf> <output.wav> [--frames N]");
        std::process::exit(2);
    };

    // Stopping after a fixed number of frames is what makes the device
    // comparable. Bench step 9 has the box print a running checksum of the PCM
    // it has decoded; playing a whole Tonie to the end is over half an hour of
    // loud audio, so the host decodes exactly as many frames as the box
    // reported and the two checksums are of the same samples.
    let mut limit: Option<usize> = None;
    if args.next().as_deref() == Some("--frames") {
        limit = match args.next().and_then(|n| n.parse().ok()) {
            Some(n) => Some(n),
            None => {
                eprintln!("--frames needs a number");
                std::process::exit(2);
            }
        };
    }

    let data = fs::read(&input).map_err(|e| format!("reading {input}: {e}"))?;
    let source = SlicePages::new(&data).map_err(|e| format!("opening {input}: {e}"))?;
    // The decoder state is ~27 KB and the decoder borrows it, so it has to
    // outlive `decoder`. On device this is a static; here, main's stack.
    let mut state = OpusState::new();
    let opus = LibOpus::new(&mut state).map_err(|e| format!("initializing Opus decoder: {e}"))?;
    let mut decoder =
        TafDecoder::open(source, opus).map_err(|e| format!("opening {input}: {e}"))?;

    println!(
        "audio id {:#010x}, {} chapters",
        decoder.header().audio_id,
        decoder.chapter_count()
    );

    let spec = hound::WavSpec {
        channels: CHANNELS as u16,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut wav =
        hound::WavWriter::create(&output, spec).map_err(|e| format!("creating {output}: {e}"))?;

    let mut pcm = [0i16; MAX_FRAME_SAMPLES];
    let mut total = 0usize;
    let mut frames = 0usize;
    loop {
        if limit.is_some_and(|max| frames >= max) {
            break;
        }
        let n = match decoder.next_frame(&mut pcm) {
            Ok(Some(n)) => n,
            Ok(None) => break,
            Err(e) => return Err(format!("decoding {input}: {e}").into()),
        };
        for &s in &pcm[..n] {
            wav.write_sample(s)
                .map_err(|e| format!("writing {output}: {e}"))?;
        }
        total += n;
        frames += 1;
    }
    wav.finalize()
        .map_err(|e| format!("finalizing {output}: {e}"))?;

    println!(
        "wrote {output}: {frames} frames, {:.2} s",
        (total / CHANNELS) as f64 / SAMPLE_RATE as f64
    );
    Ok(())
}
