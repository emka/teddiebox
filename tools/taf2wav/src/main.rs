//! Decodes a .taf to a .wav. The M1 demonstration, and the tool used to
//! listen to anything the device later claims it cannot play.

use std::fs;

use teddiebox_audio::{LibOpus, OpusState, TafDecoder, CHANNELS, MAX_FRAME_SAMPLES, SAMPLE_RATE};
use teddiebox_taf::SlicePages;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let (Some(input), Some(output)) = (args.next(), args.next()) else {
        eprintln!("usage: taf2wav <input.taf> <output.wav>");
        std::process::exit(2);
    };

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
    loop {
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
    }
    wav.finalize()
        .map_err(|e| format!("finalizing {output}: {e}"))?;

    println!(
        "wrote {output}: {:.2} s",
        (total / CHANNELS) as f64 / SAMPLE_RATE as f64
    );
    Ok(())
}
