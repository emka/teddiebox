//! Where a download's bytes go, and how far it has got.
//!
//! The watermark this keeps is what the reader is gated on, so it counts only
//! bytes the sink accepted. Counting optimistically would clear the decoder to
//! read pages that never reached the card.

use crate::units::Bytes;

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

/// How far a download has got, and when its bytes are made durable.
///
/// The sink is passed in per write rather than owned, because on device it
/// cannot be owned: the media task is handed the card for the length of one
/// call so that a download in flight never holds a borrow the rest of the
/// loop needs. Only the counting outlives a call, which is exactly what this
/// holds — rebuilding it around each sink would restart the flush interval
/// every time and the flush would never arrive.
pub struct Writer {
    watermark: u32,
    since_flush: u32,
    flush_every: u32,
}

impl Writer {
    /// `already_on_card` is where a resumed download continues from; zero for
    /// a fresh one. `flush_every` trades a directory-entry write against how
    /// much of an interrupted download is lost.
    pub fn resuming(already_on_card: u32, flush_every: u32) -> Self {
        Self {
            watermark: already_on_card,
            since_flush: 0,
            flush_every,
        }
    }

    pub fn write<S: ContentSink>(&mut self, sink: &mut S, bytes: &[u8]) -> Result<(), S::Error> {
        sink.append(bytes)?;
        self.watermark += bytes.len() as u32;
        self.since_flush += bytes.len() as u32;
        if self.since_flush >= self.flush_every {
            sink.flush()?;
            self.since_flush = 0;
        }
        Ok(())
    }

    /// Flushes the tail. The last bytes are the ones that make the file
    /// complete, and completeness is read back off the card.
    pub fn finish<S: ContentSink>(&mut self, sink: &mut S) -> Result<(), S::Error> {
        if self.since_flush > 0 {
            sink.flush()?;
            self.since_flush = 0;
        }
        Ok(())
    }

    /// Bytes committed. This is what the reader is gated on.
    pub fn watermark(&self) -> Bytes {
        Bytes(self.watermark)
    }
}

#[cfg(test)]
mod tests {
    use super::{ContentSink, Writer};
    use crate::units::Bytes;
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
        let mut card = Recording::default();
        let mut w = Writer::resuming(0, 1024);
        w.write(&mut card, b"0123456789").unwrap();
        assert_eq!(w.watermark(), Bytes(10));
    }

    /// A resumed download's watermark starts at what is already on the card,
    /// or the reader would be told the file is shorter than it is and would
    /// refuse to decode pages that are sitting right there.
    #[test]
    fn a_resumed_download_counts_from_what_is_already_there() {
        let mut card = Recording::default();
        let mut w = Writer::resuming(4096, 1024);
        assert_eq!(w.watermark(), Bytes(4096));
        w.write(&mut card, b"XYZ").unwrap();
        assert_eq!(w.watermark(), Bytes(4099));
    }

    #[test]
    fn the_bytes_reach_the_sink_unchanged_and_in_order() {
        let mut card = Recording::default();
        let mut w = Writer::resuming(0, 1024);
        w.write(&mut card, b"abc").unwrap();
        w.write(&mut card, b"def").unwrap();
        assert_eq!(card.bytes, b"abcdef");
    }

    /// Flushing is what makes the on-card length truthful, and a resume reads
    /// that length. Flushing every write would cost a directory-entry write
    /// per chunk; never flushing loses the whole download to a flat battery.
    #[test]
    fn a_flush_happens_once_the_interval_is_passed_and_not_before() {
        let mut card = Recording::default();
        let mut w = Writer::resuming(0, 100);
        w.write(&mut card, &[0u8; 60]).unwrap();
        assert_eq!(card.flushes, 0, "60 bytes is not yet 100");
        w.write(&mut card, &[0u8; 60]).unwrap();
        assert_eq!(card.flushes, 1);
    }

    #[test]
    fn the_flush_interval_restarts_after_each_flush() {
        let mut card = Recording::default();
        let mut w = Writer::resuming(0, 100);
        w.write(&mut card, &[0u8; 250]).unwrap();
        assert_eq!(
            card.flushes, 1,
            "one write past the interval is one flush, not two"
        );
        w.write(&mut card, &[0u8; 60]).unwrap();
        assert_eq!(
            card.flushes, 1,
            "the 150-byte overshoot is discarded; after a flush the interval restarts from zero, so 60 is not yet 100"
        );
        w.write(&mut card, &[0u8; 60]).unwrap();
        assert_eq!(card.flushes, 2);
    }

    /// The last bytes of a download are the ones that make the file complete,
    /// and a completeness check reads the length off the card.
    #[test]
    fn finishing_flushes_whatever_is_left() {
        let mut card = Recording::default();
        let mut w = Writer::resuming(0, 1024);
        w.write(&mut card, b"tail").unwrap();
        w.finish(&mut card).unwrap();
        assert_eq!(card.flushes, 1);
    }

    /// Nothing to make durable, nothing to write: the interval has just been
    /// flushed, so the file's recorded length is already truthful and another
    /// directory-entry write would say the same thing twice.
    #[test]
    fn finishing_on_the_flush_boundary_writes_nothing_further() {
        let mut card = Recording::default();
        let mut w = Writer::resuming(0, 100);
        w.write(&mut card, &[0u8; 100]).unwrap();
        assert_eq!(card.flushes, 1);
        w.finish(&mut card).unwrap();
        assert_eq!(card.flushes, 1);
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
        let mut w = Writer::resuming(0, 1024);
        assert!(w.write(&mut Broken, b"abc").is_err());
        assert_eq!(w.watermark(), Bytes(0));
    }

    /// On the box the sink cannot be owned for the length of a download: the
    /// media task is handed the card for one call at a time, so that a
    /// download in flight never holds a borrow the rest of the loop needs.
    /// The cadence therefore has to outlive every sink it writes through. A
    /// writer that owned its sink would have to be rebuilt for each call, and
    /// a rebuilt writer starts its interval again from zero — with 512-byte
    /// chunks and a megabyte interval, the flush would then never come at all.
    #[test]
    fn the_flush_interval_survives_a_sink_that_lives_for_one_call_only() {
        struct ForOneCall<'a>(&'a mut Recording);

        impl ContentSink for ForOneCall<'_> {
            type Error = ();
            fn append(&mut self, bytes: &[u8]) -> Result<(), ()> {
                self.0.append(bytes)
            }
            fn flush(&mut self) -> Result<(), ()> {
                self.0.flush()
            }
        }

        let mut card = Recording::default();
        let mut w = Writer::resuming(0, 100);

        w.write(&mut ForOneCall(&mut card), &[0u8; 60]).unwrap();
        assert_eq!(card.flushes, 0, "60 bytes is not yet 100");

        w.write(&mut ForOneCall(&mut card), &[0u8; 60]).unwrap();
        assert_eq!(
            card.flushes, 1,
            "the second call continues the first call's interval"
        );
    }

    /// The design promises to "stop, and never leave a sidecar claiming a
    /// file is complete" when the card fails or fills. That promise is about
    /// the sidecar's length claim, not about the watermark: the bytes an
    /// `append` accepted are genuinely on the card even if the flush that
    /// would make their length durable then fails. Rolling the watermark
    /// back here would stall the decoder on pages that are really present,
    /// and would silently retire bytes that a retry should still flush.
    #[test]
    fn a_failed_flush_still_leaves_the_appended_bytes_on_the_watermark() {
        #[derive(Default)]
        struct AppendsButNeverFlushes {
            appends: usize,
            flushes: usize,
        }

        impl ContentSink for AppendsButNeverFlushes {
            type Error = ();
            fn append(&mut self, _: &[u8]) -> Result<(), ()> {
                self.appends += 1;
                Ok(())
            }
            fn flush(&mut self) -> Result<(), ()> {
                self.flushes += 1;
                Err(())
            }
        }

        let mut card = AppendsButNeverFlushes::default();
        let mut w = Writer::resuming(0, 100);

        // The append succeeds and reaches the interval, so a flush is
        // attempted and fails; write() reports that failure.
        assert!(w.write(&mut card, &[0u8; 100]).is_err());
        assert_eq!(card.appends, 1);
        assert_eq!(card.flushes, 1);
        assert_eq!(
            w.watermark(),
            Bytes(100),
            "the appended bytes are on the card"
        );

        // The failed flush did not reset the interval counter, so the very
        // next write retries the flush immediately rather than waiting out
        // a fresh 100 bytes.
        assert!(w.write(&mut card, &[0u8; 1]).is_err());
        assert_eq!(
            card.flushes, 2,
            "a failed flush must be retried on the next write"
        );
    }
}
