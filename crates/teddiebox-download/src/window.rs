//! Serving pages that have arrived, and refusing the ones that have not.
//!
//! Two things live here. The **gate** is what the media loop asks before
//! decoding another frame; keeping it in the loop rather than inside
//! `read_page` is what lets [`PageSource`] stay synchronous and `TafReader`
//! stay untouched. The **window** is a read-ahead batch, and it exists because
//! `embedded-sdmmc` restarts a file's cluster walk from its first cluster on
//! any backward seek — so a batch of eight turns one walk per 280 ms of audio
//! into one per two seconds.

use teddiebox_taf::{PageSource, PAGE_SIZE};

/// Whether the decoder may be asked for another frame.
///
/// `watermark` and `next_page` are both counted in **pages**. `margin_pages`
/// is how many whole pages must be committed beyond the one about to be read,
/// so that a frame spanning a page boundary does not run into bytes that have
/// not arrived.
pub fn may_decode(next_page: u32, watermark: u32, margin_pages: u32, complete: bool) -> bool {
    // A finished file has nothing left to wait for; gating it would stall the
    // last frames of every story.
    complete || watermark >= next_page.saturating_add(margin_pages).saturating_add(1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowError<E> {
    /// The download has not reached this page yet. Not a fault: the caller
    /// waits and asks again.
    NotYetDownloaded,
    Inner(E),
}

/// A `PageSource` that serves only what the download has committed, a batch at
/// a time.
///
/// **This is large.** At `BATCH = 8` the cache alone is 32 KB, which is a
/// third of what the playback cushion and the decode scratch already cost on a
/// device with no PSRAM. On the host it lives wherever a test puts it; on the
/// device it must be constructed *directly into* its final location using an
/// in-place initialiser such as `StaticCell::init_with(|| Window::new(src))`,
/// not built on a task's stack. (`new` returns by value, so
/// `let w = Window::new(src)` materialises 32 KB inline, and return-place
/// elision is not guaranteed.)
pub struct Window<P, const BATCH: usize> {
    inner: P,
    cache: [[u8; PAGE_SIZE]; BATCH],
    /// First page held in `cache`, and how many of them are valid.
    first: u32,
    held: u32,
    /// Committed bytes, from the writer.
    watermark: u32,
    reads: u32,
    batches: u32,
}

impl<P: PageSource, const BATCH: usize> Window<P, BATCH> {
    pub fn new(inner: P) -> Self {
        Self {
            inner,
            cache: [[0u8; PAGE_SIZE]; BATCH],
            first: 0,
            held: 0,
            watermark: 0,
            reads: 0,
            batches: 0,
        }
    }

    /// Tells the window how far the download has committed, in bytes.
    pub fn set_watermark(&mut self, bytes: u32) {
        self.watermark = bytes;
    }

    /// Whole pages the download has committed.
    pub fn pages_available(&self) -> u32 {
        self.watermark / PAGE_SIZE as u32
    }

    /// How many times the inner source was asked for a page. Test support and
    /// bench instrumentation.
    pub fn reads(&self) -> u32 {
        self.reads
    }

    /// How many times a batch was filled — one backward seek each.
    pub fn batches(&self) -> u32 {
        self.batches
    }

    fn fill_from(&mut self, page: u32) -> Result<(), WindowError<P::Error>> {
        let available = self.pages_available();
        // Never read past what the download committed: those pages hold
        // whatever the card had there before, which decodes as noise or as the
        // end of a previous story. Saturating to defend this safety property
        // locally, not relying on the caller's guard.
        let wanted = available.saturating_sub(page).min(BATCH as u32);
        self.held = 0;
        self.first = page;
        self.batches += 1;
        for slot in 0..wanted {
            let mut buf = [0u8; PAGE_SIZE];
            self.inner
                .read_page(page + slot, &mut buf)
                .map_err(WindowError::Inner)?;
            self.reads += 1;
            self.cache[slot as usize] = buf;
            self.held += 1;
        }
        Ok(())
    }
}

impl<P: PageSource, const BATCH: usize> PageSource for Window<P, BATCH> {
    type Error = WindowError<P::Error>;

    fn read_page(&mut self, index: u32, buf: &mut [u8; PAGE_SIZE]) -> Result<(), Self::Error> {
        // A page is available only once the watermark has passed all of it.
        if index >= self.pages_available() {
            return Err(WindowError::NotYetDownloaded);
        }

        let held = index >= self.first && index < self.first + self.held;
        if !held {
            self.fill_from(index)?;
        }

        let slot = (index - self.first) as usize;
        buf.copy_from_slice(&self.cache[slot]);
        Ok(())
    }

    fn page_count(&self) -> u32 {
        self.inner.page_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pages whose every byte is the page's own index, so a test can tell at a
    /// glance which page it was handed.
    struct Numbered {
        pages: u32,
        reads: u32,
    }

    impl PageSource for Numbered {
        type Error = ();
        fn read_page(&mut self, index: u32, buf: &mut [u8; PAGE_SIZE]) -> Result<(), ()> {
            if index >= self.pages {
                return Err(());
            }
            self.reads += 1;
            buf.fill(index as u8);
            Ok(())
        }
        fn page_count(&self) -> u32 {
            self.pages
        }
    }

    fn window(pages: u32) -> Window<Numbered, 8> {
        Window::new(Numbered { pages, reads: 0 })
    }

    #[test]
    fn a_page_the_download_has_reached_is_served() {
        let mut w = window(16);
        w.set_watermark(4 * PAGE_SIZE as u32);
        let mut buf = [0u8; PAGE_SIZE];
        w.read_page(0, &mut buf).unwrap();
        assert_eq!(buf[0], 0);
        assert_eq!(buf[PAGE_SIZE - 1], 0);
    }

    /// The failure this type exists to prevent: handing the decoder a page of
    /// zeroes, or of the previous story, because the bytes have not arrived.
    #[test]
    fn a_page_beyond_the_watermark_is_refused_rather_than_guessed() {
        let mut w = window(16);
        w.set_watermark(2 * PAGE_SIZE as u32);
        let mut buf = [0u8; PAGE_SIZE];
        assert_eq!(w.read_page(2, &mut buf), Err(WindowError::NotYetDownloaded));
    }

    /// A page is only whole once the watermark has passed all of it. Serving
    /// a half-written page is the same defect as serving one not started.
    #[test]
    fn a_page_only_half_written_is_not_yet_available() {
        let mut w = window(16);
        w.set_watermark(PAGE_SIZE as u32 + 1);
        let mut buf = [0u8; PAGE_SIZE];
        assert!(w.read_page(0, &mut buf).is_ok());
        assert_eq!(w.read_page(1, &mut buf), Err(WindowError::NotYetDownloaded));
    }

    /// The whole point of the batch: eight sequential pages cost one trip to
    /// the card, so one backward FAT walk covers ~2 s of audio.
    #[test]
    fn eight_sequential_pages_cost_one_trip_to_the_card() {
        let mut w = window(64);
        w.set_watermark(64 * PAGE_SIZE as u32);
        let mut buf = [0u8; PAGE_SIZE];
        for page in 0..8 {
            w.read_page(page, &mut buf).unwrap();
            assert_eq!(buf[0], page as u8, "page {page} served wrong content");
        }
        assert_eq!(w.reads(), 8, "a batch is filled with eight reads");
        assert_eq!(w.batches(), 1, "but only one seek back to fill it");
    }

    #[test]
    fn a_ninth_page_starts_a_new_batch() {
        let mut w = window(64);
        w.set_watermark(64 * PAGE_SIZE as u32);
        let mut buf = [0u8; PAGE_SIZE];
        for page in 0..9 {
            w.read_page(page, &mut buf).unwrap();
        }
        assert_eq!(w.batches(), 2);
    }

    /// A batch must never reach past the watermark, or it reads pages that do
    /// not exist yet and caches whatever the card happens to hold there.
    #[test]
    fn a_batch_stops_at_the_watermark() {
        let mut w = window(64);
        w.set_watermark(3 * PAGE_SIZE as u32);
        let mut buf = [0u8; PAGE_SIZE];
        w.read_page(0, &mut buf).unwrap();
        assert_eq!(w.reads(), 3, "three pages exist; the batch stops there");
    }

    /// Playing a file while it downloads: batch truncated by watermark, then
    /// watermark advances, then the next page arrives and can be served.
    #[test]
    fn a_batch_truncated_by_watermark_extends_as_watermark_advances() {
        let mut w = window(64);
        w.set_watermark(3 * PAGE_SIZE as u32);
        let mut buf = [0u8; PAGE_SIZE];
        // First read fills a batch, truncated to three pages.
        w.read_page(0, &mut buf).unwrap();
        assert_eq!(w.reads(), 3);
        // Page 3 is not yet available.
        assert_eq!(w.read_page(3, &mut buf), Err(WindowError::NotYetDownloaded));
        // Watermark advances, page 3 and beyond now exist.
        w.set_watermark(8 * PAGE_SIZE as u32);
        // Page 3 is fetched (and the batch refilled from page 3).
        w.read_page(3, &mut buf).unwrap();
        assert_eq!(buf[0], 3, "page 3 content delivered");
    }

    #[test]
    fn a_page_already_in_the_batch_costs_no_trip_at_all() {
        let mut w = window(64);
        w.set_watermark(64 * PAGE_SIZE as u32);
        let mut buf = [0u8; PAGE_SIZE];
        w.read_page(0, &mut buf).unwrap();
        let after_first = w.reads();
        w.read_page(3, &mut buf).unwrap();
        assert_eq!(buf[0], 3);
        assert_eq!(w.reads(), after_first, "already cached");
    }

    #[test]
    fn a_page_before_the_batch_refills_from_there() {
        let mut w = window(64);
        w.set_watermark(64 * PAGE_SIZE as u32);
        let mut buf = [0u8; PAGE_SIZE];
        w.read_page(10, &mut buf).unwrap();
        w.read_page(2, &mut buf).unwrap();
        assert_eq!(buf[0], 2);
        assert_eq!(w.batches(), 2);
    }

    #[test]
    fn the_inner_sources_error_is_reported_as_its_own() {
        let mut w: Window<Numbered, 8> = Window::new(Numbered { pages: 0, reads: 0 });
        w.set_watermark(PAGE_SIZE as u32);
        let mut buf = [0u8; PAGE_SIZE];
        assert_eq!(w.read_page(0, &mut buf), Err(WindowError::Inner(())));
    }

    // --- the gate ---

    /// Page 4 with a margin of 4 needs pages 0..=8 committed — the page
    /// itself, plus four whole pages beyond it — so the watermark must be 9.
    #[test]
    fn decoding_waits_until_the_margin_is_covered() {
        assert!(!may_decode(4, 8, 4, false), "eight pages is one short");
        assert!(may_decode(4, 9, 4, false));
    }

    /// Once the file is whole there is nothing left to wait for, and gating
    /// would stall the last frames of every story.
    #[test]
    fn a_complete_file_is_never_gated() {
        assert!(may_decode(1000, 0, 4, true));
    }

    #[test]
    fn a_margin_of_zero_still_requires_the_page_itself() {
        assert!(
            !may_decode(4, 4, 0, false),
            "page 4 needs 5 pages committed"
        );
        assert!(may_decode(4, 5, 0, false));
    }

    /// A watermark near u32::MAX must not wrap the margin into a small number
    /// and let the decoder through.
    #[test]
    fn the_margin_does_not_overflow_near_the_end_of_the_range() {
        assert!(!may_decode(u32::MAX, 10, 4, false));
    }
}
