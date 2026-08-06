//! Measures what one Opus packet costs to decode.
//!
//! Real-time margin is the project's go/no-go question, and M1 produced no
//! number for a device measurement to be compared against. This is that
//! number, taken against the same fixed-point libopus the device will run,
//! so the two are comparable rather than merely similar.
//!
//! Throughput is declared in samples per channel, so criterion's `elem/s`
//! reads directly as a sampling rate: divide it by 48 000 to get the
//! real-time factor. A device that decodes at 1x is exactly keeping up and
//! has no margin for the SD card, the DAC, or anything else.
//!
//! Not run in CI. A wall-clock assertion on a shared runner is a flaky
//! assertion, and a test nobody trusts provides no feedback.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use teddiebox_audio::{LibOpus, OpusDecode, OpusState, CHANNELS, MAX_FRAME_SAMPLES};
use teddiebox_taf::{SlicePages, TafReader, MAX_PACKET};

const FIXTURE: &[u8] = include_bytes!("../../teddiebox-taf/tests/data/sine.taf");

/// TAF encodes 60 ms frames, which at 48 kHz is 2 880 samples per channel.
const SAMPLES_PER_PACKET: usize = 2880;

/// Every audio packet in the fixture, headers excluded.
fn audio_packets() -> Vec<Vec<u8>> {
    let mut reader = TafReader::open(SlicePages::new(FIXTURE).unwrap()).unwrap();
    let mut scratch = [0u8; MAX_PACKET];
    let mut packets = Vec::new();

    while let Some(len) = reader.next_packet(&mut scratch).unwrap() {
        let packet = &scratch[..len];
        if packet.starts_with(b"OpusHead") || packet.starts_with(b"OpusTags") {
            continue;
        }
        packets.push(packet.to_vec());
    }
    packets
}

fn decode_one_packet(c: &mut Criterion) {
    let packets = audio_packets();
    assert!(!packets.is_empty(), "fixture yielded no audio packets");

    let mut state = OpusState::new();
    let mut decoder = LibOpus::new(&mut state).unwrap();
    let mut pcm = [0i16; MAX_FRAME_SAMPLES];

    let mut group = c.benchmark_group("opus");
    group.throughput(Throughput::Elements(SAMPLES_PER_PACKET as u64));

    // Cycling through the fixture's packets rather than repeating one keeps
    // the decoder's state evolving the way it does during playback, so the
    // measurement is not of a single unusually warm code path.
    let mut next = 0usize;
    group.bench_function(BenchmarkId::new("decode", "60ms stereo frame"), |b| {
        b.iter(|| {
            let packet = &packets[next % packets.len()];
            next += 1;
            let n = decoder.decode(packet, &mut pcm).unwrap();
            assert_eq!(n, SAMPLES_PER_PACKET * CHANNELS);
            n
        })
    });
    group.finish();
}

criterion_group!(benches, decode_one_packet);
criterion_main!(benches);
