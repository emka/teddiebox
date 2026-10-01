//! Where a download's bytes go, and how far it has got.
//!
//! The reader is gated on the watermark kept here, so it only counts bytes the
//! sink accepted. Otherwise the decoder could read pages that never reached
//! the card.

use crate::units::Bytes;

/// Somewhere a download's bytes can be appended.
///
/// **`append` must write at the end of the file**, whatever the handle's
/// current offset. On the device the reader and writer share one file handle
/// (`embedded-sdmmc` cannot open a file twice), so the offset moves between
/// calls and the implementation must seek to the end itself.
pub trait ContentSink {
    type Error;
    fn append(&mut self, bytes: &[u8]) -> Result<(), Self::Error>;
    /// Saves the bytes written so far and updates the file's recorded length,
    /// which a later resume reads.
    fn flush(&mut self) -> Result<(), Self::Error>;
}

/// How far a download has got, and when its bytes are made durable.
///
/// The sink is passed to each call rather than owned: on the device the card
/// is only borrowed for one call at a time. This struct keeps the counters
/// between calls; rebuilding it for each call would restart the flush
/// interval, and the flush would never happen.
pub struct Writer {
    watermark: u32,
    since_flush: u32,
    flush_every: u32,
}

impl Writer {
    /// `already_on_card` is where a resumed download continues; zero for a new
    /// one. `flush_every` balances directory-entry writes against how much of
    /// an interrupted download is lost.
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

    /// Flushes the last bytes, so the file's length on the card is complete.
    pub fn finish<S: ContentSink>(&mut self, sink: &mut S) -> Result<(), S::Error> {
        if self.since_flush > 0 {
            sink.flush()?;
            self.since_flush = 0;
        }
        Ok(())
    }

    /// Bytes written. The reader is gated on this.
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
        // Given
        let mut card = Recording::default();
        let mut w = Writer::resuming(0, 1024);

        // When
        w.write(&mut card, b"0123456789").unwrap();

        // Then
        assert_eq!(w.watermark(), Bytes(10));
    }

    /// A resumed download's watermark starts at what is already on the card,
    /// or the reader would refuse pages that are already there.
    #[test]
    fn a_resumed_download_counts_from_what_is_already_there() {
        // Given
        let mut card = Recording::default();
        let mut w = Writer::resuming(4096, 1024);
        let before = w.watermark();

        // When
        w.write(&mut card, b"XYZ").unwrap();

        // Then
        assert_eq!(before, Bytes(4096));
        assert_eq!(w.watermark(), Bytes(4099));
    }

    #[test]
    fn the_bytes_reach_the_sink_unchanged_and_in_order() {
        // Given
        let mut card = Recording::default();
        let mut w = Writer::resuming(0, 1024);

        // When
        w.write(&mut card, b"abc").unwrap();
        w.write(&mut card, b"def").unwrap();

        // Then
        assert_eq!(card.bytes, b"abcdef");
    }

    /// Flushing updates the length on the card, which a resume reads.
    /// Flushing on every write would be slow; never flushing would lose the
    /// whole download on a power loss.
    #[test]
    fn a_flush_happens_once_the_interval_is_passed_and_not_before() {
        // Given: a flush every 100 bytes
        let mut card = Recording::default();
        let mut w = Writer::resuming(0, 100);

        // When
        w.write(&mut card, &[0u8; 60]).unwrap();
        let flushes_after_60 = card.flushes;
        w.write(&mut card, &[0u8; 60]).unwrap();

        // Then
        assert_eq!(flushes_after_60, 0, "60 bytes is not yet 100");
        assert_eq!(card.flushes, 1);
    }

    #[test]
    fn the_flush_interval_restarts_after_each_flush() {
        // Given: a flush every 100 bytes
        let mut card = Recording::default();
        let mut w = Writer::resuming(0, 100);

        // When
        w.write(&mut card, &[0u8; 250]).unwrap();
        let flushes_after_250 = card.flushes;
        w.write(&mut card, &[0u8; 60]).unwrap();
        let flushes_after_310 = card.flushes;
        w.write(&mut card, &[0u8; 60]).unwrap();

        // Then
        assert_eq!(
            flushes_after_250, 1,
            "one write past the interval is one flush, not two"
        );
        assert_eq!(
            flushes_after_310, 1,
            "the 150-byte overshoot is discarded; after a flush the interval restarts from zero, so 60 is not yet 100"
        );
        assert_eq!(card.flushes, 2);
    }

    /// The last bytes complete the file, and the completeness check reads the
    /// length from the card.
    #[test]
    fn finishing_flushes_whatever_is_left() {
        // Given
        let mut card = Recording::default();
        let mut w = Writer::resuming(0, 1024);
        w.write(&mut card, b"tail").unwrap();

        // When
        w.finish(&mut card).unwrap();

        // Then
        assert_eq!(card.flushes, 1);
    }

    /// Nothing new since the last flush, so no extra write.
    #[test]
    fn finishing_on_the_flush_boundary_writes_nothing_further() {
        // Given
        let mut card = Recording::default();
        let mut w = Writer::resuming(0, 100);
        w.write(&mut card, &[0u8; 100]).unwrap();
        let flushes_before = card.flushes;

        // When
        w.finish(&mut card).unwrap();

        // Then
        assert_eq!(flushes_before, 1);
        assert_eq!(card.flushes, 1);
    }

    /// A failed write must not move the watermark, or the reader could
    /// decode bytes that never reached the card.
    #[test]
    fn a_failed_write_does_not_advance_the_watermark() {
        // Given
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

        // When
        let result = w.write(&mut Broken, b"abc");

        // Then
        assert!(result.is_err());
        assert_eq!(w.watermark(), Bytes(0));
    }

    /// On the box the card is borrowed for one call at a time, so the flush
    /// interval must carry over between calls. Otherwise, with 512-byte
    /// chunks and a megabyte interval, the flush would never happen.
    #[test]
    fn the_flush_interval_survives_a_sink_that_lives_for_one_call_only() {
        // Given
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

        // When
        w.write(&mut ForOneCall(&mut card), &[0u8; 60]).unwrap();
        let flushes_after_first_call = card.flushes;
        w.write(&mut ForOneCall(&mut card), &[0u8; 60]).unwrap();

        // Then
        assert_eq!(flushes_after_first_call, 0, "60 bytes is not yet 100");
        assert_eq!(
            card.flushes, 1,
            "the second call continues the first call's interval"
        );
    }

    /// Bytes that `append` accepted are on the card even if the flush then
    /// fails, so the watermark keeps them. Rolling it back would stall the
    /// decoder on pages that are there.
    #[test]
    fn a_failed_flush_still_leaves_the_appended_bytes_on_the_watermark() {
        // Given
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

        // When: the append reaches the interval, so a flush is tried and
        // fails; then one more byte arrives
        let first = w.write(&mut card, &[0u8; 100]);
        let (appends, flushes, watermark) = (card.appends, card.flushes, w.watermark());
        let second = w.write(&mut card, &[0u8; 1]);

        // Then
        assert!(first.is_err());
        assert_eq!(appends, 1);
        assert_eq!(flushes, 1);
        assert_eq!(watermark, Bytes(100), "the appended bytes are on the card");
        // The failed flush did not reset the counter, so the next write
        // tries the flush again straight away.
        assert!(second.is_err());
        assert_eq!(
            card.flushes, 2,
            "a failed flush must be retried on the next write"
        );
    }
}
