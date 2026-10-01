//! Serves pages that have been downloaded, and refuses those that have not.
//!
//! Two parts. The **gate** ([`may_decode`]) is checked by the media loop
//! before decoding each frame; doing it there keeps [`PageSource`]
//! synchronous. The **window** reads pages in batches, because
//! `embedded-sdmmc` walks the file's cluster chain from the start on every
//! backward seek. A batch of eight means one walk per two seconds of audio
//! instead of one per 280 ms.

use crate::units::{Bytes, Pages};
use teddiebox_taf::{PageSource, PAGE_SIZE};

/// Whether the decoder may be asked for another frame.
///
/// `next_page` and `watermark` are both counted in pages (see
/// [`Window::pages_available`] to convert from the writer's byte count).
/// `margin` is how many whole pages must be written beyond the one about to be
/// read, so a frame that crosses a page boundary has its bytes.
pub fn may_decode(next_page: Pages, watermark: Pages, margin: Pages, complete: bool) -> bool {
    // A finished file has nothing to wait for; gating it would stall the last
    // frames of every story.
    complete || watermark.0 >= next_page.0.saturating_add(margin.0).saturating_add(1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowError<E> {
    /// The download has not reached this page yet. Not a fault: the caller
    /// waits and asks again.
    NotYetDownloaded,
    Inner(E),
}

/// A `PageSource` that serves only pages the download has written, a batch
/// at a time.
///
/// **This is large.** At `BATCH = 8` the cache alone is 32 KB. On the device
/// it must be built directly in its final place, for example with
/// `StaticCell::init_with(|| Window::new(src))`, not on a task's stack:
/// `let w = Window::new(src)` would put 32 KB on the stack.
pub struct Window<P, const BATCH: usize> {
    inner: P,
    cache: [[u8; PAGE_SIZE]; BATCH],
    /// First page held in `cache`, and how many of them are valid.
    first: u32,
    held: u32,
    /// Bytes written so far, from the writer.
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

    /// Tells the window how many bytes the download has written.
    pub fn set_watermark(&mut self, watermark: Bytes) {
        self.watermark = watermark.0;
    }

    /// Whole pages the download has written.
    pub fn pages_available(&self) -> Pages {
        Bytes(self.watermark).whole_pages()
    }

    /// How many times the inner source was asked for a page. For tests and
    /// diagnostics.
    pub fn reads(&self) -> u32 {
        self.reads
    }

    /// How many times a batch was filled — one backward seek each.
    pub fn batches(&self) -> u32 {
        self.batches
    }

    fn fill_from(&mut self, page: u32) -> Result<(), WindowError<P::Error>> {
        let available = self.pages_available().0;
        // Never read past what the download has written: those pages hold old
        // data from the card. `saturating_sub` keeps this safe even if the
        // caller did not check.
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
        if index >= self.pages_available().0 {
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

    /// Every byte of a page is the page's index, so a test can see which page
    /// it got.
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
        w.set_watermark(Bytes(4 * PAGE_SIZE as u32));
        let mut buf = [0u8; PAGE_SIZE];
        w.read_page(0, &mut buf).unwrap();
        assert_eq!(buf[0], 0);
        assert_eq!(buf[PAGE_SIZE - 1], 0);
    }

    /// A page that has not been downloaded must not be served.
    #[test]
    fn a_page_beyond_the_watermark_is_refused_rather_than_guessed() {
        let mut w = window(16);
        w.set_watermark(Bytes(2 * PAGE_SIZE as u32));
        let mut buf = [0u8; PAGE_SIZE];
        assert_eq!(w.read_page(2, &mut buf), Err(WindowError::NotYetDownloaded));
    }

    /// A page is only available once all of it is written.
    #[test]
    fn a_page_only_half_written_is_not_yet_available() {
        let mut w = window(16);
        w.set_watermark(Bytes(PAGE_SIZE as u32 + 1));
        let mut buf = [0u8; PAGE_SIZE];
        assert!(w.read_page(0, &mut buf).is_ok());
        assert_eq!(w.read_page(1, &mut buf), Err(WindowError::NotYetDownloaded));
    }

    /// Eight pages in a row cost one batch, so one backward FAT walk covers
    /// about 2 s of audio.
    #[test]
    fn eight_sequential_pages_cost_one_trip_to_the_card() {
        let mut w = window(64);
        w.set_watermark(Bytes(64 * PAGE_SIZE as u32));
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
        w.set_watermark(Bytes(64 * PAGE_SIZE as u32));
        let mut buf = [0u8; PAGE_SIZE];
        for page in 0..9 {
            w.read_page(page, &mut buf).unwrap();
        }
        assert_eq!(w.batches(), 2);
    }

    /// A batch must stop at the watermark, or it would cache old data from
    /// the card.
    #[test]
    fn a_batch_stops_at_the_watermark() {
        let mut w = window(64);
        w.set_watermark(Bytes(3 * PAGE_SIZE as u32));
        let mut buf = [0u8; PAGE_SIZE];
        w.read_page(0, &mut buf).unwrap();
        assert_eq!(w.reads(), 3, "three pages exist; the batch stops there");
    }

    /// Playing a file while it downloads: the batch stops at the watermark,
    /// then the watermark moves on and the next page can be served.
    #[test]
    fn a_batch_truncated_by_watermark_extends_as_watermark_advances() {
        let mut w = window(64);
        w.set_watermark(Bytes(3 * PAGE_SIZE as u32));
        let mut buf = [0u8; PAGE_SIZE];
        // First read fills a batch, truncated to three pages.
        w.read_page(0, &mut buf).unwrap();
        assert_eq!(w.reads(), 3);
        // Page 3 is not yet available.
        assert_eq!(w.read_page(3, &mut buf), Err(WindowError::NotYetDownloaded));
        // Watermark advances, page 3 and beyond now exist.
        w.set_watermark(Bytes(8 * PAGE_SIZE as u32));
        // Page 3 is fetched (and the batch refilled from page 3).
        w.read_page(3, &mut buf).unwrap();
        assert_eq!(buf[0], 3, "page 3 content delivered");
    }

    #[test]
    fn a_page_already_in_the_batch_costs_no_trip_at_all() {
        let mut w = window(64);
        w.set_watermark(Bytes(64 * PAGE_SIZE as u32));
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
        w.set_watermark(Bytes(64 * PAGE_SIZE as u32));
        let mut buf = [0u8; PAGE_SIZE];
        w.read_page(10, &mut buf).unwrap();
        w.read_page(2, &mut buf).unwrap();
        assert_eq!(buf[0], 2);
        assert_eq!(w.batches(), 2);
    }

    #[test]
    fn the_inner_sources_error_is_reported_as_its_own() {
        let mut w: Window<Numbered, 8> = Window::new(Numbered { pages: 0, reads: 0 });
        w.set_watermark(Bytes(PAGE_SIZE as u32));
        let mut buf = [0u8; PAGE_SIZE];
        assert_eq!(w.read_page(0, &mut buf), Err(WindowError::Inner(())));
    }

    #[test]
    fn the_window_has_as_many_pages_as_the_source_it_wraps() {
        // Given
        let w = window(16);

        // When
        let count = w.page_count();

        // Then
        assert_eq!(count, 16);
    }

    // --- the gate ---

    /// Page 4 with a margin of 4 needs pages 0..=8 written (the page plus
    /// four more), so the watermark must be 9.
    #[test]
    fn decoding_waits_until_the_margin_is_covered() {
        assert!(
            !may_decode(Pages(4), Pages(8), Pages(4), false),
            "eight pages is one short"
        );
        assert!(may_decode(Pages(4), Pages(9), Pages(4), false));
    }

    /// A complete file is never gated, or the last frames of every story
    /// would stall.
    #[test]
    fn a_complete_file_is_never_gated() {
        assert!(may_decode(Pages(1000), Pages(0), Pages(4), true));
    }

    #[test]
    fn a_margin_of_zero_still_requires_the_page_itself() {
        assert!(
            !may_decode(Pages(4), Pages(4), Pages(0), false),
            "page 4 needs 5 pages committed"
        );
        assert!(may_decode(Pages(4), Pages(5), Pages(0), false));
    }

    /// A watermark near u32::MAX must not wrap the margin into a small number
    /// and let the decoder through.
    #[test]
    fn the_margin_does_not_overflow_near_the_end_of_the_range() {
        assert!(!may_decode(Pages(u32::MAX), Pages(10), Pages(4), false));
    }
}
