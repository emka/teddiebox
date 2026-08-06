//! Decodes a .taf to a .wav. The M1 demonstration, and the tool used to
//! listen to anything the device later claims it cannot play.

use std::fs;

use teddiebox_audio::{LibOpus, TafDecoder, CHANNELS, MAX_FRAME_SAMPLES, SAMPLE_RATE};
use teddiebox_taf::SlicePages;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let (Some(input), Some(output)) = (args.next(), args.next()) else {
        eprintln!("usage: taf2wav <input.taf> <output.wav>");
        std::process::exit(2);
    };

    let data = fs::read(&input)?;
    let mut decoder = TafDecoder::open(SlicePages::new(&data)?, LibOpus::new()?)?;

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
    let mut wav = hound::WavWriter::create(&output, spec)?;

    let mut pcm = [0i16; MAX_FRAME_SAMPLES];
    let mut total = 0usize;
    while let Some(n) = decoder.next_frame(&mut pcm)? {
        for &s in &pcm[..n] {
            wav.write_sample(s)?;
        }
        total += n;
    }
    wav.finalize()?;

    println!(
        "wrote {output}: {:.2} s",
        (total / CHANNELS) as f64 / SAMPLE_RATE as f64
    );
    Ok(())
}
