//! How much of the stack has ever been used.
//!
//! Three experiments this session have guessed at this number and two of them
//! cost a bench session: a second core given 32 KiB panicked on its stack
//! guard, and an audio buffer enlarged until only 42 KiB of stack remained left
//! the box silent and needing a cold boot. The number was never measured, only
//! assumed, and both assumptions were wrong in the same direction.
//!
//! So: paint the free stack with a known word at boot, and count backwards from
//! the top later to find the deepest point anything reached. It is the standard
//! trick and it costs one pass over free memory at startup.
//!
//! **This measures the main task's stack**, which on `esp-hal` is whatever DRAM
//! `.bss` leaves over — the region between `_stack_end` and `_stack_start`.
//! Every embassy task polled by the thread-mode executor runs on it, so the
//! figure is the worst case across all of them, not any one task's own need.

use core::sync::atomic::{AtomicUsize, Ordering};

unsafe extern "C" {
    /// Lowest address the stack may grow into. From `esp-hal`'s `stack.x`.
    static _stack_end: u32;
    /// One past the highest address the stack occupies. It grows *down* from
    /// here.
    static _stack_start: u32;
}

/// The word written into free stack. Chosen to be implausible as data: a
/// pointer, a length or a sample would all have to be exactly this to be
/// mistaken for paint.
const PAINT: u32 = 0xC0DE_FACE;

/// How much room to leave below the stack pointer when painting.
///
/// Painting over the frame that is doing the painting would corrupt the return
/// address of the very call doing it. A kilobyte is far more than the few words
/// this needs and costs only a slightly pessimistic measurement.
const HEADROOM: usize = 1024;

/// Where painting stopped, so the reader knows what was never covered.
static PAINTED_TO: AtomicUsize = AtomicUsize::new(0);

/// Fills the unused stack with [`PAINT`].
///
/// Call once, as early in `main` as possible: everything below the caller's
/// frame is free at that moment, and anything already used before this runs is
/// invisible to the measurement afterwards.
pub fn paint() {
    let low = &raw const _stack_end as usize;
    // A local's address is a good enough stand-in for the stack pointer, and
    // it does not need inline assembly to obtain.
    let here = {
        let probe = 0u32;
        &probe as *const u32 as usize
    };
    let stop = here.saturating_sub(HEADROOM);
    if stop <= low {
        return;
    }

    let mut at = low;
    while at < stop {
        // SAFETY: `low` comes from the linker's own stack bounds and `stop` is
        // below this frame, so the whole range is stack memory that nothing is
        // using. Both ends are word-aligned because the region is.
        unsafe { (at as *mut u32).write_volatile(PAINT) };
        at += 4;
    }
    PAINTED_TO.store(stop, Ordering::Relaxed);
}

/// The deepest point the stack has reached, in bytes used.
///
/// `None` if [`paint`] never ran, or if the paint is gone all the way down —
/// which means the stack went at least as deep as the painting reached and the
/// true figure is unknown rather than merely large. Reporting a floor as if it
/// were a measurement is how this number got guessed wrong twice already.
pub fn high_water() -> Option<Used> {
    let painted_to = PAINTED_TO.load(Ordering::Relaxed);
    if painted_to == 0 {
        return None;
    }
    let low = &raw const _stack_end as usize;
    let high = &raw const _stack_start as usize;

    let mut at = low;
    while at < painted_to {
        // SAFETY: within the painted range, which is stack memory.
        if unsafe { (at as *const u32).read_volatile() } != PAINT {
            break;
        }
        at += 4;
    }

    Some(Used {
        bytes: high.saturating_sub(at),
        total: high.saturating_sub(low),
        exhausted: at >= painted_to,
    })
}

/// A stack measurement.
pub struct Used {
    /// Bytes between the deepest point reached and the top of the stack.
    pub bytes: usize,
    /// The whole stack, for comparison.
    pub total: usize,
    /// The paint was gone everywhere it was applied, so `bytes` is a floor and
    /// not the answer.
    pub exhausted: bool,
}
