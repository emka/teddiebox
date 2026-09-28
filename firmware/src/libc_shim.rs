//! The C library symbols that libopus's hardening checks need.
//!
//! libopus is built with hardening on, on purpose: the decoder reads files
//! from an SD card, and stopping on a malformed one is worth it. Hardening
//! makes `celt_fatal` reachable, which needs `fprintf`, `abort` and (through
//! `stderr`) newlib's `__getreent`; `_FORTIFY_SOURCE` adds the `__*_chk`
//! functions.
//!
//! Linking newlib instead would pull `malloc`, stdio and unimplemented
//! syscall stubs into a `no_std` image. `esp-rtos` does not provide
//! `__getreent` either (only with its `alloc` feature, via a ROM table).
//!
//! Defining them here also improves the failure: instead of `abort()`, a
//! hardening failure becomes a Rust panic, and `esp-backtrace` prints a
//! backtrace on the console.

use core::ffi::{c_char, c_int, c_void};

/// Where libopus ends up if a hardening check fails.
///
/// Reaching this means the decoder found a broken internal invariant, in
/// practice a corrupt or malicious file. Panicking leaves a backtrace.
#[unsafe(no_mangle)]
extern "C" fn abort() -> ! {
    panic!("libopus aborted: a hardening check failed while decoding");
}

/// Enough of newlib's reentrancy block for `stderr` to be read out of it.
///
/// `stderr` expands to `__getreent()->_stderr`, so this pointer is read
/// before `fprintf` is called. Only the first few words (`_stdin`, `_stdout`,
/// `_stderr`) are read; they are null, and `fprintf` below ignores them.
static mut REENT: [usize; 32] = [0; 32];

#[unsafe(no_mangle)]
extern "C" fn __getreent() -> *mut c_void {
    // Only read, and only just before a panic, so there are no aliasing
    // issues.
    core::ptr::addr_of_mut!(REENT) as *mut c_void
}

/// Swallows the message libopus would print before aborting.
///
/// Declared without varargs, which works because the arguments are never
/// read: `celt_fatal` calls [`abort`] next, which panics.
#[unsafe(no_mangle)]
extern "C" fn fprintf(_stream: *mut c_void, _format: *const c_char) -> c_int {
    0
}

/// `memcpy` with the destination size known, from `_FORTIFY_SOURCE`.
///
/// Implemented, not stubbed: the bounds check *is* the hardening.
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
    // SAFETY: `len` fits the destination (checked above), and the caller
    // upholds `memcpy`'s contract: both regions valid for `len` bytes and not
    // overlapping.
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
    // SAFETY: `len` fits the destination (checked above), and the caller
    // upholds `memset`'s contract: `dest` writable for `len` bytes.
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
    // SAFETY: `len` fits the destination (checked above), and the caller
    // upholds `memmove`'s contract: both regions valid for `len` bytes.
    unsafe { core::ptr::copy(src as *const u8, dest as *mut u8, len) };
    dest
}
