//! The one `FlashStorage` handle, lent out to whoever needs raw flash access.
//!
//! A device-wide resource, not an OTA one: it lives here rather than in
//! `ota.rs` only because OTA needed it first. `identity::load` reads a data
//! partition through the same handle and has nothing to do with OTA at all.

use core::sync::atomic::{AtomicBool, Ordering};
use esp_bootloader_esp_idf::partitions::FlashStorage;

/// The one flash handle, built on first use and kept for the life of the box.
///
/// `FlashStorage::new` is documented as panicking if called twice. What it
/// actually does on this chip is quieter and worse: the first handle works and
/// every later one fails every read with `StorageError`, so a command that
/// worked once starts reporting a flash that looks broken. Found on
/// 2026-09-16 by running `otas` twice — the second call failed, and so did
/// every flash access after it until the box was rebooted.
///
/// So it is built once. Constructing per command was the bug.
static mut FLASH: Option<FlashStorage<'static>> = None;

/// Whether the one handle is currently lent out.
///
/// What makes [`flash`] sound. Every caller runs on the one executor and
/// none holds the handle across an `.await` — every function that borrows it
/// is synchronous — so no two borrows can overlap, whichever task each is in.
/// That is true today and enforced by nothing but this: a second borrow while the first is alive
/// panics where it happens instead of quietly producing two `&mut` to the same
/// peripheral. It cannot fire while the invariant holds, and if the invariant
/// stops holding, a panic names the moment it stopped.
static LENT: AtomicBool = AtomicBool::new(false);

/// The one flash handle, borrowed. Returns itself on drop.
pub(crate) struct Flash(&'static mut FlashStorage<'static>);

impl Drop for Flash {
    fn drop(&mut self) {
        LENT.store(false, Ordering::Release);
    }
}

impl core::ops::Deref for Flash {
    type Target = FlashStorage<'static>;

    fn deref(&self) -> &Self::Target {
        self.0
    }
}

impl core::ops::DerefMut for Flash {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0
    }
}

/// Hands out the one flash handle.
///
/// Callers are the first line of `main` ([`crate::ota::confirm_boot_or_revert`]),
/// the main loop's confirmation check ([`crate::ota::mark_valid`]), the
/// console's OTA commands ([`crate::ota::status`], [`crate::ota::write_probe`],
/// [`crate::ota::arm_boot`]), [`crate::identity::load`] and
/// [`crate::wifikey::load`] once at boot, and [`crate::wifikey`]'s store from
/// the net task's idle pass — each synchronous, so each finishes with the
/// handle before anything else on the executor can ask for it.
pub(crate) fn flash() -> Flash {
    assert!(
        !LENT.swap(true, Ordering::Acquire),
        "the flash handle is already lent out"
    );
    // SAFETY: `LENT` was false and this thread has just set it, so no other
    // `&mut` to `FLASH` exists. It goes back to false only when the `Flash`
    // holding that reference is dropped.
    unsafe {
        let slot = &mut *core::ptr::addr_of_mut!(FLASH);
        Flash(slot.get_or_insert_with(|| FlashStorage::new(esp_hal::peripherals::FLASH::steal())))
    }
}
