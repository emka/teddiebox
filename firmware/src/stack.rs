//! How much of the stack has ever been used.
//!
//! Needed before changing anything that shares RAM with the stack; guessing
//! has gone wrong before.
//!
//! The free stack is filled with a known word at boot. Later, the untouched
//! words show how deep the stack has ever reached. This costs one pass over
//! free memory at start-up.
//!
//! **This measures the main stack**, which on `esp-hal` is the DRAM left
//! after `.bss` (between `_stack_end` and `_stack_start`). All embassy tasks
//! on the thread-mode executor share it, so the figure is the worst case
//! across all of them.

use core::sync::atomic::{AtomicUsize, Ordering};

unsafe extern "C" {
    /// Lowest address the stack may grow into. From `esp-hal`'s `stack.x`.
    static _stack_end: u32;
    /// One past the highest address the stack occupies. It grows *down* from
    /// here.
    static _stack_start: u32;
    /// The stack canary, at `_stack_end + ESP_HAL_CONFIG_STACK_GUARD_OFFSET`.
    ///
    /// **Never write here.** With `stack_guard_monitoring` the chip has a
    /// watchpoint on this word, so any write traps at once, and the box then
    /// needs a J100 recovery.
    static __stack_chk_guard: u32;
}

/// The word written into free stack. Chosen to be unlikely as real data.
const PAINT: u32 = 0xC0DE_FACE;

/// How much room to leave below the stack pointer when painting.
///
/// Painting over the current stack frame would corrupt its return address. A
/// kilobyte is plenty, and only makes the measurement slightly pessimistic.
const HEADROOM: usize = 1024;

/// Where painting stopped, so the reader knows what was never covered.
static PAINTED_TO: AtomicUsize = AtomicUsize::new(0);

/// Fills the unused stack with [`PAINT`].
///
/// Call once, as early in `main` as possible: everything below the caller's
/// frame is free then, and stack used before this call is not measured.
pub fn paint() {
    // Start *above* the canary, never at `_stack_end`, or the write traps.
    let low = (&raw const __stack_chk_guard as usize) + 4;
    let floor = &raw const _stack_end as usize;
    let ceiling = &raw const _stack_start as usize;
    // Give up if the symbols look wrong: a wrong address would overwrite live
    // memory and crash the box silently.
    if low <= floor || low >= ceiling {
        return;
    }
    // A local variable's address is close enough to the stack pointer, and
    // needs no inline assembly.
    let here = {
        let probe = 0u32;
        &probe as *const u32 as usize
    };
    let stop = here.saturating_sub(HEADROOM);
    if stop <= low || stop > ceiling {
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
/// `None` if [`paint`] never ran, or if all the paint is gone: then the stack
/// went at least as deep as the painting, and the true figure is unknown.
pub fn high_water() -> Option<Used> {
    let painted_to = PAINTED_TO.load(Ordering::Relaxed);
    if painted_to == 0 {
        return None;
    }
    let low = (&raw const __stack_chk_guard as usize) + 4;
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

/// Prints the high-water mark, or says plainly that there is not one.
///
/// Used by the `stack` console command and by the setup portal (which never
/// reaches the console loop), so both print the same text.
pub fn report() {
    match high_water() {
        None => esp_println::println!("teddiebox: stack was never painted"),
        Some(used) if used.exhausted => {
            // Only a lower bound, not the real figure.
            esp_println::println!(
                "teddiebox: stack at least {} of {} bytes — the paint is gone \
                 everywhere, so this is a floor",
                used.bytes,
                used.total
            );
        }
        Some(used) => esp_println::println!(
            "teddiebox: stack deepest {} of {} bytes, {} spare",
            used.bytes,
            used.total,
            used.total.saturating_sub(used.bytes)
        ),
    }
}

/// A stack measurement.
pub struct Used {
    /// Bytes between the deepest point reached and the top of the stack.
    pub bytes: usize,
    /// The stack from the canary up, which is all of it that may be used.
    pub total: usize,
    /// The paint was gone everywhere it was applied, so `bytes` is a floor and
    /// not the answer.
    pub exhausted: bool,
}
