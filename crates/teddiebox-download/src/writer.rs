//! Where a download's bytes go, and how far it has got.
//!
//! The watermark this keeps is what the reader is gated on, so it counts only
//! bytes the sink accepted. Counting optimistically would clear the decoder to
//! read pages that never reached the card.

/// Somewhere a download's bytes can be appended.
///
/// **`append` must write at the end of the file**, whatever the underlying
/// handle's offset happens to be. On device the reader and the writer share
/// one file handle — `embedded-sdmmc` will not open a file twice — so the
/// offset moves between calls, and an implementation seeks to the end itself.
/// Saying so in the contract is what stops it being forgotten in a loop that
/// otherwise looks right.
pub trait ContentSink {
    type Error;
    fn append(&mut self, bytes: &[u8]) -> Result<(), Self::Error>;
    /// Makes the bytes written so far durable, and the file's recorded length
    /// truthful — which is what a later resume reads.
    fn flush(&mut self) -> Result<(), Self::Error>;
}

pub struct Writer<S> {
    sink: S,
    watermark: u32,
    since_flush: u32,
    flush_every: u32,
}

impl<S: ContentSink> Writer<S> {
    /// `already_on_card` is where a resumed download continues from; zero for
    /// a fresh one. `flush_every` trades a directory-entry write against how
    /// much of an interrupted download is lost.
    pub fn resuming(sink: S, already_on_card: u32, flush_every: u32) -> Self {
        Self {
            sink,
            watermark: already_on_card,
            since_flush: 0,
            flush_every,
        }
    }

    pub fn write(&mut self, bytes: &[u8]) -> Result<(), S::Error> {
        self.sink.append(bytes)?;
        self.watermark += bytes.len() as u32;
        self.since_flush += bytes.len() as u32;
        if self.since_flush >= self.flush_every {
            self.sink.flush()?;
            self.since_flush = 0;
        }
        Ok(())
    }

    /// Flushes the tail. The last bytes are the ones that make the file
    /// complete, and completeness is read back off the card.
    pub fn finish(&mut self) -> Result<(), S::Error> {
        if self.since_flush > 0 {
            self.sink.flush()?;
            self.since_flush = 0;
        }
        Ok(())
    }

    /// Bytes committed. This is what the reader is gated on.
    pub fn watermark(&self) -> u32 {
        self.watermark
    }

    pub fn sink(&self) -> &S {
        &self.sink
    }

    pub fn into_sink(self) -> S {
        self.sink
    }
}

#[cfg(test)]
mod tests {
    use super::{ContentSink, Writer};
    extern crate std;
    use std::vec::Vec;

    #[derive(Default)]
    struct Recording {
        bytes: Vec<u8>,
        flushes: usize,
    }

    impl ContentSink for Recording {
        type Error = ();
        fn append(&mut self, bytes: &[u8]) -> Result<(), ()> {
            self.bytes.extend_from_slice(bytes);
            Ok(())
        }
        fn flush(&mut self) -> Result<(), ()> {
            self.flushes += 1;
            Ok(())
        }
    }

    #[test]
    fn the_watermark_counts_every_byte_written() {
        let mut w = Writer::resuming(Recording::default(), 0, 1024);
        w.write(b"0123456789").unwrap();
        assert_eq!(w.watermark(), 10);
    }

    /// A resumed download's watermark starts at what is already on the card,
    /// or the reader would be told the file is shorter than it is and would
    /// refuse to decode pages that are sitting right there.
    #[test]
    fn a_resumed_download_counts_from_what_is_already_there() {
        let mut w = Writer::resuming(Recording::default(), 4096, 1024);
        assert_eq!(w.watermark(), 4096);
        w.write(b"XYZ").unwrap();
        assert_eq!(w.watermark(), 4099);
    }

    #[test]
    fn the_bytes_reach_the_sink_unchanged_and_in_order() {
        let mut w = Writer::resuming(Recording::default(), 0, 1024);
        w.write(b"abc").unwrap();
        w.write(b"def").unwrap();
        assert_eq!(w.into_sink().bytes, b"abcdef");
    }

    /// Flushing is what makes the on-card length truthful, and a resume reads
    /// that length. Flushing every write would cost a directory-entry write
    /// per chunk; never flushing loses the whole download to a flat battery.
    #[test]
    fn a_flush_happens_once_the_interval_is_passed_and_not_before() {
        let mut w = Writer::resuming(Recording::default(), 0, 100);
        w.write(&[0u8; 60]).unwrap();
        assert_eq!(w.sink().flushes, 0, "60 bytes is not yet 100");
        w.write(&[0u8; 60]).unwrap();
        assert_eq!(w.sink().flushes, 1);
    }

    #[test]
    fn the_flush_interval_restarts_after_each_flush() {
        let mut w = Writer::resuming(Recording::default(), 0, 100);
        w.write(&[0u8; 250]).unwrap();
        assert_eq!(
            w.sink().flushes,
            1,
            "one write past the interval is one flush, not two"
        );
        w.write(&[0u8; 60]).unwrap();
        assert_eq!(
            w.sink().flushes,
            1,
            "the 150-byte overshoot is discarded; after a flush the interval restarts from zero, so 60 is not yet 100"
        );
        w.write(&[0u8; 60]).unwrap();
        assert_eq!(w.sink().flushes, 2);
    }

    /// The last bytes of a download are the ones that make the file complete,
    /// and a completeness check reads the length off the card.
    #[test]
    fn finishing_flushes_whatever_is_left() {
        let mut w = Writer::resuming(Recording::default(), 0, 1024);
        w.write(b"tail").unwrap();
        w.finish().unwrap();
        assert_eq!(w.sink().flushes, 1);
    }

    /// A failed write must not advance the watermark: the reader would then
    /// be cleared to decode bytes that never reached the card.
    #[test]
    fn a_failed_write_does_not_advance_the_watermark() {
        struct Broken;
        impl ContentSink for Broken {
            type Error = ();
            fn append(&mut self, _: &[u8]) -> Result<(), ()> {
                Err(())
            }
            fn flush(&mut self) -> Result<(), ()> {
                Ok(())
            }
        }
        let mut w = Writer::resuming(Broken, 0, 1024);
        assert!(w.write(b"abc").is_err());
        assert_eq!(w.watermark(), 0);
    }
}
