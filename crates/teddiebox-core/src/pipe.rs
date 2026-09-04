//! A fixed-capacity byte queue between one producer and one consumer.
//!
//! The download and the card are on opposite sides of a deadline. Bytes arrive
//! from the network in whatever sizes TLS hands over, at whatever moment the
//! server sends them; they leave towards the card only in the gaps between
//! audio frames. Neither side can wait for the other — a media loop that
//! awaited the network would underrun the moment the server paused, and a
//! network task that awaited the card would stall the connection behind a
//! decode.
//!
//! So this sits between them and absorbs the difference. **It never blocks and
//! never fails**: a write into a full pipe reports how much it took, and a read
//! from an empty one reports zero. Running dry is not an error, because the
//! download's watermark gate already knows what to do about it — pause
//! decoding until more has arrived — and running full is not an error either,
//! because the producer simply asks the server for less.

/// Bytes waiting to move from the network to the card.
///
/// Sized by the caller. One producer and one consumer are assumed: the counts
/// are plain fields, not atomics, so the whole thing lives behind whatever
/// lock the firmware already uses rather than pretending to be lock-free.
#[derive(Debug)]
pub struct Pipe<const N: usize> {
    bytes: [u8; N],
    /// Where the next byte will be read from.
    head: usize,
    /// How many bytes are queued.
    len: usize,
}

impl<const N: usize> Default for Pipe<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Pipe<N> {
    pub const fn new() -> Self {
        Self {
            bytes: [0; N],
            head: 0,
            len: 0,
        }
    }

    /// How many bytes are waiting.
    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// How many more bytes would fit.
    pub const fn free(&self) -> usize {
        N - self.len
    }

    /// Queues as much of `bytes` as fits, and says how much that was.
    ///
    /// A short write is the ordinary way this reports back-pressure, so the
    /// caller must use the return value: dropping it silently loses the tail
    /// of a download, which then fails a checksum a long way from here.
    #[must_use = "a short write means the rest of these bytes were not taken"]
    pub fn write(&mut self, bytes: &[u8]) -> usize {
        let taken = bytes.len().min(self.free());
        for &byte in &bytes[..taken] {
            let at = (self.head + self.len) % N;
            self.bytes[at] = byte;
            self.len += 1;
        }
        taken
    }

    /// Fills `out` with as much as is queued, and says how much that was.
    #[must_use = "a short read means fewer bytes were available than asked for"]
    pub fn read(&mut self, out: &mut [u8]) -> usize {
        let given = out.len().min(self.len);
        for slot in out.iter_mut().take(given) {
            *slot = self.bytes[self.head];
            self.head = (self.head + 1) % N;
            self.len -= 1;
        }
        given
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_goes_in_comes_out_in_order() {
        let mut pipe: Pipe<8> = Pipe::new();
        assert_eq!(pipe.write(b"abcd"), 4);
        let mut out = [0u8; 4];
        assert_eq!(pipe.read(&mut out), 4);
        assert_eq!(&out, b"abcd");
        assert!(pipe.is_empty());
    }

    /// The producer's back-pressure signal. A caller that ignores it drops the
    /// middle of a download and finds out at the checksum.
    #[test]
    fn a_write_that_does_not_fit_takes_what_it_can_and_says_so() {
        let mut pipe: Pipe<4> = Pipe::new();
        assert_eq!(pipe.write(b"abcdef"), 4);
        assert_eq!(pipe.free(), 0);
        assert_eq!(pipe.write(b"gh"), 0, "a full pipe takes nothing");

        let mut out = [0u8; 6];
        assert_eq!(pipe.read(&mut out), 4);
        assert_eq!(
            &out[..4],
            b"abcd",
            "and it kept the first four, not the last"
        );
    }

    /// Running dry is the normal state between server bursts, not a fault.
    #[test]
    fn reading_an_empty_pipe_yields_nothing_rather_than_failing() {
        let mut pipe: Pipe<8> = Pipe::new();
        let mut out = [0u8; 4];
        assert_eq!(pipe.read(&mut out), 0);
    }

    /// The bug this type exists to not have. After a partial read the queue
    /// starts partway along the array, so the next write wraps past the end —
    /// and a naive implementation returns those bytes in the wrong order or
    /// overwrites ones it has not handed over yet.
    #[test]
    fn bytes_survive_wrapping_around_the_end_of_the_buffer() {
        let mut pipe: Pipe<8> = Pipe::new();
        assert_eq!(pipe.write(b"abcdef"), 6);

        let mut first = [0u8; 4];
        assert_eq!(pipe.read(&mut first), 4);
        assert_eq!(&first, b"abcd");

        // Two bytes queued, head sitting at index 4: this wraps.
        assert_eq!(pipe.write(b"ghijk"), 5);
        assert_eq!(pipe.len(), 7);

        let mut rest = [0u8; 7];
        assert_eq!(pipe.read(&mut rest), 7);
        assert_eq!(&rest, b"efghijk", "the wrap reordered or clobbered bytes");
    }

    /// Many small writes and one big read is the shape this actually sees:
    /// TLS hands over records, the card takes pages.
    #[test]
    fn many_small_writes_drain_as_one_run() {
        let mut pipe: Pipe<16> = Pipe::new();
        for chunk in [b"ab".as_slice(), b"cde", b"f", b"ghij"] {
            assert_eq!(pipe.write(chunk), chunk.len());
        }
        let mut out = [0u8; 16];
        assert_eq!(pipe.read(&mut out), 10);
        assert_eq!(&out[..10], b"abcdefghij");
    }

    /// And the reverse: one big write drained in card-sized bites, repeatedly,
    /// so the indices keep wrapping rather than wrapping once.
    #[test]
    fn a_pipe_stays_correct_over_many_wraps() {
        let mut pipe: Pipe<8> = Pipe::new();
        let mut expected = 0u8;
        let mut next = 0u8;
        for _ in 0..50 {
            let batch: [u8; 5] = core::array::from_fn(|i| next.wrapping_add(i as u8));
            let taken = pipe.write(&batch);
            next = next.wrapping_add(taken as u8);

            let mut out = [0u8; 3];
            let given = pipe.read(&mut out);
            for &byte in &out[..given] {
                assert_eq!(byte, expected, "stream came out of order");
                expected = expected.wrapping_add(1);
            }
        }
    }
}
