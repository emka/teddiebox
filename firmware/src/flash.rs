//! The one `FlashStorage` handle, lent out to whoever needs raw flash access.
//!
//! Shared by OTA, `identity::load` and `wifikey`.

use core::sync::atomic::{AtomicBool, Ordering};
use esp_bootloader_esp_idf::partitions::FlashStorage;

/// The one flash handle, built on first use and kept for the life of the box.
///
/// `FlashStorage::new` is documented to panic if called twice. On this chip,
/// in practice, the second handle fails every read with `StorageError`, and
/// flash access stays broken until a reboot. So it is created only once.
static mut FLASH: Option<FlashStorage<'static>> = None;

/// Whether the one handle is currently lent out.
///
/// This makes [`flash`] safe. Every caller runs on the one executor and none
/// holds the handle across an `.await` (every function that borrows it is
/// synchronous), so two borrows never overlap. If that ever changes, a second
/// borrow panics instead of creating two `&mut` to the same peripheral.
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
/// Callers: the main loop's check ([`crate::ota::mark_valid`]), the console's
/// OTA commands ([`crate::ota::status`], [`crate::ota::write_probe`],
/// [`crate::ota::arm_boot`]), [`crate::identity::load`] and
/// [`crate::wifikey::load`] at boot, and [`crate::wifikey`]'s store from the
/// network task. All are synchronous, so each is done with the handle before
/// another can ask for it.
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
