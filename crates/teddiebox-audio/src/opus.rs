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
