//! The three C library symbols libopus's fatal path needs.
//!
//! libopus is built with hardening on, which is deliberate: the decoder parses
//! files off an SD card, and a check that stops on a malformed one is worth
//! keeping. Hardening makes `celt_fatal` reachable from the decode path, so it
//! has to link, and it reaches for `fprintf`, `abort` and — through `stderr` —
//! newlib's `__getreent`.
//!
//! Linking newlib to satisfy them was tried and is the wrong trade: it drags
//! `malloc`, stdio and a tail of unimplemented syscall stubs into a `no_std`
//! image, for a function that should never run. `esp-rtos` only wires
//! `__getreent` into a ROM syscall table, and only with its `alloc` feature,
//! so it does not supply the symbol either.
//!
//! Supplying them here keeps newlib out and makes the failure *better* than
//! libopus intended: instead of `abort()`, a hardening failure becomes a Rust
//! panic, which on this box means `esp-backtrace` prints a backtrace over the
//! console.

use core::ffi::{c_char, c_int, c_void};

/// Where libopus ends up if a hardening check fails.
///
/// Reaching this means the decoder found an internal invariant broken — in
/// practice, a corrupt or malicious file. Panicking says so and leaves a
/// backtrace; `abort()` would just stop.
#[unsafe(no_mangle)]
extern "C" fn abort() -> ! {
    panic!("libopus aborted: a hardening check failed while decoding");
}

/// Enough of newlib's reentrancy block for `stderr` to be read out of it.
///
/// `stderr` expands to `__getreent()->_stderr`, so the pointer this returns is
/// dereferenced before `fprintf` is ever called. Only the first few words are
/// read — `_stdin`, `_stdout` and `_stderr` sit at the front of the struct —
/// and they read as null, which the `fprintf` below then ignores.
static mut REENT: [usize; 32] = [0; 32];

#[unsafe(no_mangle)]
extern "C" fn __getreent() -> *mut c_void {
    // Only ever read, and only on a path that panics immediately afterwards,
    // so no aliasing or ordering question arises.
    core::ptr::addr_of_mut!(REENT) as *mut c_void
}

/// Swallows the message libopus would print before aborting.
///
/// Declared without varargs, which C's calling convention tolerates here
/// because nothing reads the arguments: the very next thing `celt_fatal` does
/// is call [`abort`], which panics with a message of its own.
#[unsafe(no_mangle)]
extern "C" fn fprintf(_stream: *mut c_void, _format: *const c_char) -> c_int {
    0
}

/// `memcpy` with the destination size known, from `_FORTIFY_SOURCE`.
///
/// Implemented rather than stubbed: the bounds check *is* the hardening, and a
/// version that skipped it would quietly turn a caught overflow back into the
/// corruption the check exists to prevent.
///
/// # Safety
///
/// The caller guarantees `dest` is writable for `len` bytes and `src` readable
/// for `len`, as for `memcpy`.
#[unsafe(no_mangle)]
unsafe extern "C" fn __memcpy_chk(
    dest: *mut c_void,
    src: *const c_void,
    len: usize,
    dest_len: usize,
) -> *mut c_void {
    if len > dest_len {
        abort();
    }
    unsafe { core::ptr::copy_nonoverlapping(src as *const u8, dest as *mut u8, len) };
    dest
}

/// `memset` with the destination size known. See [`__memcpy_chk`].
///
/// # Safety
///
/// The caller guarantees `dest` is writable for `len` bytes, as for `memset`.
#[unsafe(no_mangle)]
unsafe extern "C" fn __memset_chk(
    dest: *mut c_void,
    value: c_int,
    len: usize,
    dest_len: usize,
) -> *mut c_void {
    if len > dest_len {
        abort();
    }
    unsafe { core::ptr::write_bytes(dest as *mut u8, value as u8, len) };
    dest
}

/// `memmove` with the destination size known. See [`__memcpy_chk`].
///
/// # Safety
///
/// The caller guarantees `dest` and `src` are valid for `len` bytes, as for
/// `memmove`; overlap is permitted.
#[unsafe(no_mangle)]
unsafe extern "C" fn __memmove_chk(
    dest: *mut c_void,
    src: *const c_void,
    len: usize,
    dest_len: usize,
) -> *mut c_void {
    if len > dest_len {
        abort();
    }
    unsafe { core::ptr::copy(src as *const u8, dest as *mut u8, len) };
    dest
}
