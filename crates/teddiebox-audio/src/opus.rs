//! The only place that knows the `opus-embedded` API.

use crate::{AudioError, OpusDecode};

use opus_embedded::{Channels, Decoder, SamplingRate};

pub struct LibOpus {
    inner: Decoder,
}

impl LibOpus {
    pub fn new() -> Result<Self, AudioError> {
        // 48 kHz stereo is what TAF always carries. The "stereo" feature on
        // opus-embedded must be enabled (see Cargo.toml) or this rejects
        // `Channels::Stereo` at runtime.
        let inner =
            Decoder::new(SamplingRate::F48k, Channels::Stereo).map_err(|_| AudioError::Decode)?;
        Ok(Self { inner })
    }
}

impl OpusDecode for LibOpus {
    fn decode(&mut self, packet: &[u8], pcm: &mut [i16]) -> Result<usize, AudioError> {
        // `decode` borrows `pcm` and hands back the filled prefix; take its
        // length and drop the borrow. The slice length is already the
        // interleaved total (samples x channels), so it needs no scaling.
        let n = self
            .inner
            .decode(packet, pcm)
            .map_err(|_| AudioError::Decode)?
            .len();
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use teddiebox_taf::{SlicePages, TafReader, MAX_PACKET};

    const FIXTURE: &[u8] = include_bytes!("../../teddiebox-taf/tests/data/sine.taf");

    #[test]
    fn decodes_the_first_real_audio_packet_to_a_full_stereo_frame() {
        let mut reader = TafReader::open(SlicePages::new(FIXTURE).unwrap()).unwrap();
        let mut scratch = [0u8; MAX_PACKET];
        reader.next_packet(&mut scratch).unwrap(); // OpusHead
        reader.next_packet(&mut scratch).unwrap(); // OpusTags
        let len = reader.next_packet(&mut scratch).unwrap().unwrap();

        let mut decoder = LibOpus::new().unwrap();
        let mut pcm = [0i16; crate::MAX_FRAME_SAMPLES];
        let n = decoder.decode(&scratch[..len], &mut pcm).unwrap();

        // 60 ms of stereo audio at 48 kHz: 2880 samples/channel x 2 channels.
        // This is the real adapter, not the stub — it pins the no-double-
        // scaling contract (`opus-embedded`'s `decode` already returns the
        // interleaved total) against genuine libopus output.
        assert_eq!(n, 5760);
    }
}
