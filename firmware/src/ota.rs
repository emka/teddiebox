//! The two OTA questions that can only be answered on the box.
//!
//! Everything else about OTA is decided in `teddiebox-ota`, on the host, with
//! a fake flash watching the arithmetic. These three commands exist because
//! two questions are about this chip and this bootloader rather than about any
//! logic, and the spec (§7a, §7b) calls both design-invalidating: the firmware
//! half is shaped differently depending on the answers, so guessing costs more
//! than asking.
//!
//! **§7b — can a sector be written while the radio is associated?** Flash
//! writes suspend the instruction cache while Wi-Fi interrupts are firing. If
//! they do not survive it, the image cannot stream into flash at all: it lands
//! on the card first, the radio comes down, and the card-to-flash copy runs
//! with nothing else going. Only the sink changes, which is what the
//! `ContentSink` seam was for. [`write_probe`] asks.
//!
//! **§7a — does this bootloader roll back?** `esp-bootloader-esp-idf`'s own
//! docs warn that its prebuilt bootloaders "might not include OTA support",
//! and automatic rollback is a separate build option on top of that. If it is
//! off, marking an image `PendingVerify` is advisory and a broken image boots
//! for ever — which is precisely the safety net the design chose.
//!
//! The cheap way to ask §7a is not to install a broken image. `OtaImageState`
//! documents that the bootloader promotes [`OtaImageState::New`] to
//! [`OtaImageState::PendingVerify`] *if and only if* auto-rollback is enabled,
//! so arming a slot as `New` and reading the state back after one boot answers
//! the question with an observation rather than a brick. [`arm_boot`] arms it
//! and [`status`] reads it back.

use embassy_time::Instant;
use esp_bootloader_esp_idf::ota::{Ota, OtaImageState};
use esp_bootloader_esp_idf::partitions::{
    self, AppPartitionSubType, DataPartitionSubType, FlashStorage, PartitionType,
    PARTITION_TABLE_MAX_LEN,
};
use teddiebox_ota::{FlashRegionLike, Sectors, SECTOR};

/// How much of a sector each write carries.
///
/// Not the whole sector in one call, for two reasons. A 4 KB buffer on the
/// stack is a quarter of the measured headroom, and — more to the point — the
/// real sink never sees a sector: it gets whatever a TLS record yields, about
/// 1.4 KB. Writing in pieces is the representative shape, not a compromise.
///
/// A multiple of four, because `esp-storage` refuses any write whose offset or
/// length is not word-aligned, and falls back to a 4 KB *stack* buffer for
/// slices that are merely unaligned in memory. Both are avoided by staying on
/// word boundaries throughout.
const CHUNK: usize = 512;

/// The byte this probe expects at `offset` in the slot.
///
/// Derived from the offset rather than constant, so that a write landing in
/// the wrong place fails the read-back instead of passing it. A constant
/// pattern cannot tell a correct write from a displaced one.
fn expected(offset: u32) -> u8 {
    (offset as u8) ^ 0x5A
}

/// Maps the library's state onto the one `teddiebox-ota` reasons about.
///
/// `teddiebox-ota` does not depend on `esp-bootloader-esp-idf` — nothing in
/// it does I/O — so this mapping is the boundary where that type turns into
/// the crate's own, and it is the only place that has to know both.
fn to_slot_state(state: OtaImageState) -> teddiebox_ota::SlotState {
    match state {
        OtaImageState::New => teddiebox_ota::SlotState::New,
        OtaImageState::PendingVerify => teddiebox_ota::SlotState::PendingVerify,
        OtaImageState::Valid | OtaImageState::Invalid | OtaImageState::Aborted => {
            teddiebox_ota::SlotState::Confirmed
        }
        OtaImageState::Undefined => teddiebox_ota::SlotState::Confirmed,
    }
}

/// Opens `otadata` and returns the running slot's own state, if there is
/// one to reason about.
///
/// `None` covers both a missing `otadata` partition and a selection this
/// firmware does not recognise (`Factory`, or an unreadable entry) — a box
/// that has never run an update looks exactly like this, and must boot
/// exactly as it always has.
fn current_state() -> Option<(AppPartitionSubType, OtaImageState)> {
    let flash = flash();
    let mut buffer = [0u8; PARTITION_TABLE_MAX_LEN];
    let table = partitions::read_partition_table(flash, &mut buffer).ok()?;
    let otadata = table
        .find_partition(PartitionType::Data(DataPartitionSubType::Ota))
        .ok()??;
    let mut ota = Ota::new(otadata.as_flash_region(flash), 2).ok()?;

    let current = match ota.current_app_partition().ok()? {
        slot @ (AppPartitionSubType::Ota0 | AppPartitionSubType::Ota1) => slot,
        _ => return None,
    };
    let state = ota.current_ota_state().ok()?;
    Some((current, state))
}

/// The first thing `main` calls, before anything that could itself crash.
///
/// Spec §7a's measurement means the bootloader will never do this for us:
/// a slot armed `New` stays `New` forever here. So this either arms the
/// one-shot confirmation (`New` -> `PendingVerify`) or, finding
/// `PendingVerify` already sitting there from a boot that never reached
/// `mark_valid`, switches back to the other slot immediately and reboots.
/// A box that has never run an update takes neither branch.
pub fn confirm_boot_or_revert() {
    let Some((current, state)) = current_state() else {
        return;
    };

    match teddiebox_ota::boot_action(to_slot_state(state)) {
        teddiebox_ota::BootAction::Proceed => {}
        teddiebox_ota::BootAction::ConfirmFirstBoot => {
            let flash = flash();
            let mut buffer = [0u8; PARTITION_TABLE_MAX_LEN];
            let table = match partitions::read_partition_table(flash, &mut buffer) {
                Ok(table) => table,
                Err(trouble) => {
                    esp_println::println!(
                        "teddiebox: ota cannot read the partition table — {trouble:?}"
                    );
                    return;
                }
            };
            let otadata = match table.find_partition(PartitionType::Data(DataPartitionSubType::Ota))
            {
                Ok(Some(entry)) => entry,
                Ok(None) => {
                    esp_println::println!(
                        "teddiebox: ota has no otadata partition — this flash cannot update"
                    );
                    return;
                }
                Err(trouble) => {
                    esp_println::println!("teddiebox: ota cannot find otadata — {trouble:?}");
                    return;
                }
            };
            let mut ota = match Ota::new(otadata.as_flash_region(flash), 2) {
                Ok(ota) => ota,
                Err(trouble) => {
                    esp_println::println!("teddiebox: ota cannot open otadata — {trouble:?}");
                    return;
                }
            };
            match ota.set_current_ota_state(OtaImageState::PendingVerify) {
                Ok(()) => esp_println::println!(
                    "teddiebox: ota first boot of this slot — armed PendingVerify"
                ),
                Err(trouble) => {
                    esp_println::println!("teddiebox: ota could not set the state — {trouble:?}")
                }
            }
        }
        teddiebox_ota::BootAction::Revert => {
            esp_println::println!(
                "teddiebox: ota {current:?} never confirmed — reverting and rebooting"
            );
            let (_, other) = slots(current);
            let flash = flash();
            let mut buffer = [0u8; PARTITION_TABLE_MAX_LEN];
            let Ok(table) = partitions::read_partition_table(flash, &mut buffer) else {
                return;
            };
            let Ok(Some(otadata)) =
                table.find_partition(PartitionType::Data(DataPartitionSubType::Ota))
            else {
                return;
            };
            let Ok(mut ota) = Ota::new(otadata.as_flash_region(flash), 2) else {
                return;
            };
            if ota.set_current_app_partition(other).is_err() {
                esp_println::println!("teddiebox: ota could not switch slots — booting on anyway");
                return;
            }
            // `set_current_app_partition` only rewrites `other`'s sequence
            // number; it says nothing about `other`'s own `ota_state`, which
            // still holds whatever that slot's otadata entry last recorded.
            // If that happens to be `PendingVerify` too — reachable by arming
            // one slot, booting it unconfirmed, then arming the other before
            // the first ever confirms — reverting *to* it would leave a slot
            // that reverts again on its own next boot, before the console
            // even comes up. Falling back to a slot is this firmware
            // re-trusting it, the same as a fresh confirmation would, so it
            // is stamped `Valid` here rather than left to whatever it said
            // before. `set_current_app_partition` leaves this same handle
            // addressing the slot it just selected, so this lands on `other`,
            // never on `current`.
            if ota.set_current_ota_state(OtaImageState::Valid).is_err() {
                esp_println::println!(
                    "teddiebox: ota could not stamp {other:?} valid — booting on anyway"
                );
                return;
            }
            crate::drain_console();
            esp_hal::system::software_reset();
        }
    }
}

/// Called once the card has mounted and the codec has come up — the things
/// a genuinely broken build breaks. Idempotent: it only ever transitions a
/// slot that is still `PendingVerify`, so calling it more than once, or on a
/// box that never ran an update, does nothing.
pub fn mark_valid() {
    let Some((_, OtaImageState::PendingVerify)) = current_state() else {
        return;
    };
    let flash = flash();
    let mut buffer = [0u8; PARTITION_TABLE_MAX_LEN];
    let table = match partitions::read_partition_table(flash, &mut buffer) {
        Ok(table) => table,
        Err(trouble) => {
            esp_println::println!("teddiebox: ota cannot read the partition table — {trouble:?}");
            return;
        }
    };
    let otadata = match table.find_partition(PartitionType::Data(DataPartitionSubType::Ota)) {
        Ok(Some(entry)) => entry,
        Ok(None) => {
            esp_println::println!(
                "teddiebox: ota has no otadata partition — this flash cannot update"
            );
            return;
        }
        Err(trouble) => {
            esp_println::println!("teddiebox: ota cannot find otadata — {trouble:?}");
            return;
        }
    };
    let mut ota = match Ota::new(otadata.as_flash_region(flash), 2) {
        Ok(ota) => ota,
        Err(trouble) => {
            esp_println::println!("teddiebox: ota cannot open otadata — {trouble:?}");
            return;
        }
    };
    match ota.set_current_ota_state(OtaImageState::Valid) {
        Ok(()) => esp_println::println!("teddiebox: ota confirmed — marked Valid"),
        Err(trouble) => {
            esp_println::println!("teddiebox: ota could not set the state — {trouble:?}")
        }
    }
}

/// `teddiebox-ota` drives the erase bookkeeping; this is the flash under it.
///
/// The adapter exists so the probe exercises the shipping code path — the same
/// [`Sectors`] that will feed a real image — rather than a second, parallel
/// one written for the bench. A spike that tests different code answers a
/// different question.
struct Region<'a, 'd>(partitions::FlashRegion<'a, 'd>);

impl FlashRegionLike for Region<'_, '_> {
    type Error = partitions::Error;

    fn erase(&mut self, range: core::ops::Range<u32>) -> Result<(), partitions::Error> {
        self.0.erase(range.start, range.end)
    }

    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), partitions::Error> {
        self.0.write(offset, bytes)
    }
}

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

/// Hands out the one flash handle.
///
/// SAFETY: every caller of this — the first line of `main`
/// ([`confirm_boot_or_revert`]), the main loop's confirmation check
/// ([`mark_valid`]), and the console's OTA commands ([`status`],
/// [`write_probe`], [`arm_boot`]) — runs in the same task, and none of them
/// holds the reference across an `.await` point. One task, never reentered
/// while a `&mut` is live, is what keeps this to one `&mut` at a time, not
/// which of those callers happens to be running.
fn flash() -> &'static mut FlashStorage<'static> {
    unsafe {
        let slot = &mut *core::ptr::addr_of_mut!(FLASH);
        slot.get_or_insert_with(|| FlashStorage::new(esp_hal::peripherals::FLASH::steal()))
    }
}

/// The slot the running image is in, and the other one.
///
/// Returned as a pair because every caller here wants both: the probe writes
/// into the slot that is *not* running, and the report names the one that is.
fn slots(booted: AppPartitionSubType) -> (&'static str, AppPartitionSubType) {
    match booted {
        AppPartitionSubType::Ota1 => ("ota_1", AppPartitionSubType::Ota0),
        _ => ("ota_0", AppPartitionSubType::Ota1),
    }
}

/// Prints which slot booted and what `otadata` says about it.
///
/// Reads and changes nothing. It is the only way to see from outside whether a
/// rollback happened or the box merely rebooted — both look identical on the
/// console otherwise — and it is how §7a's answer is read back after
/// [`arm_boot`].
pub fn status() {
    let flash = flash();
    let mut buffer = [0u8; PARTITION_TABLE_MAX_LEN];
    let table = match partitions::read_partition_table(flash, &mut buffer) {
        Ok(table) => table,
        Err(trouble) => {
            esp_println::println!("teddiebox: ota cannot read the partition table — {trouble:?}");
            return;
        }
    };

    match table.booted_partition() {
        Ok(Some(entry)) => esp_println::println!(
            "teddiebox: ota booted {} at {:#x}, {} bytes",
            entry.label_as_str(),
            entry.offset(),
            entry.len()
        ),
        Ok(None) => esp_println::println!("teddiebox: ota booted an unrecognised partition"),
        Err(trouble) => {
            esp_println::println!("teddiebox: ota cannot name the booted slot — {trouble:?}")
        }
    }

    let otadata = match table.find_partition(PartitionType::Data(DataPartitionSubType::Ota)) {
        Ok(Some(entry)) => entry,
        Ok(None) => {
            esp_println::println!(
                "teddiebox: ota has no otadata partition — this flash cannot update"
            );
            return;
        }
        Err(trouble) => {
            esp_println::println!("teddiebox: ota cannot find otadata — {trouble:?}");
            return;
        }
    };

    let mut ota = match Ota::new(otadata.as_flash_region(flash), 2) {
        Ok(ota) => ota,
        Err(trouble) => {
            esp_println::println!("teddiebox: ota cannot open otadata — {trouble:?}");
            return;
        }
    };

    // Both are printed even when one fails: a blank `otadata` answers the
    // first and errors the second, and that combination is itself the
    // expected state of a flash that has never been updated.
    match ota.current_app_partition() {
        Ok(slot) => esp_println::println!("teddiebox: ota otadata selects {slot:?}"),
        Err(trouble) => {
            esp_println::println!("teddiebox: ota otadata selects nothing — {trouble:?}")
        }
    }
    match ota.current_ota_state() {
        Ok(state) => esp_println::println!("teddiebox: ota state {state:?}"),
        Err(trouble) => esp_println::println!("teddiebox: ota state unreadable — {trouble:?}"),
    }
}

/// Erases, writes and reads back one sector of the slot that is not running.
///
/// Spec §7b. Run it three ways — radio down, radio associated and idle, then
/// associated under download load — and the answer is whichever of those the
/// read-back stops surviving.
///
/// The slot that is *not* running is the target on purpose: it is where a real
/// update writes, so a pass here is about the operation the firmware half will
/// actually perform. Writing into a scratch region would answer an easier
/// question than the one asked.
pub fn write_probe() {
    let flash = flash();
    let mut buffer = [0u8; PARTITION_TABLE_MAX_LEN];
    let table = match partitions::read_partition_table(flash, &mut buffer) {
        Ok(table) => table,
        Err(trouble) => {
            esp_println::println!("teddiebox: ota cannot read the partition table — {trouble:?}");
            return;
        }
    };

    let booted = match table.booted_partition() {
        Ok(Some(entry)) => entry,
        _ => {
            esp_println::println!("teddiebox: ota cannot tell which slot is running — not writing");
            return;
        }
    };
    let running = if booted.label_as_str() == "ota_1" {
        AppPartitionSubType::Ota1
    } else {
        AppPartitionSubType::Ota0
    };
    let (running_name, target) = slots(running);

    let entry = match table.find_partition(PartitionType::App(target)) {
        Ok(Some(entry)) => entry,
        Ok(None) => {
            esp_println::println!("teddiebox: ota has no second app slot — nothing to write into");
            return;
        }
        Err(trouble) => {
            esp_println::println!("teddiebox: ota cannot find the spare slot — {trouble:?}");
            return;
        }
    };
    let slot_bytes = entry.len();
    let label = entry.label_as_str();
    esp_println::println!(
        "teddiebox: ota probing {label} ({slot_bytes} bytes) while running from {running_name}"
    );

    let mut region = Region(entry.as_flash_region(flash));
    let mut sectors = Sectors::new(slot_bytes);
    let started = Instant::now();

    let mut chunk = [0u8; CHUNK];
    let mut offset = 0u32;
    while offset < SECTOR {
        for (index, byte) in chunk.iter_mut().enumerate() {
            *byte = expected(offset + index as u32);
        }
        if let Err(trouble) = sectors.feed(&mut region, offset, &chunk) {
            esp_println::println!("teddiebox: ota write failed at {offset} — {trouble:?}");
            return;
        }
        offset += CHUNK as u32;
    }
    let written = started.elapsed().as_millis();

    // Read back through the partition rather than the adapter: the adapter
    // deliberately has no read, because the sink it models never needs one.
    let mut verify = [0u8; CHUNK];
    let mut offset = 0u32;
    while offset < SECTOR {
        if let Err(trouble) = region.0.read(offset, &mut verify) {
            esp_println::println!("teddiebox: ota read-back failed at {offset} — {trouble:?}");
            return;
        }
        for (index, &byte) in verify.iter().enumerate() {
            let want = expected(offset + index as u32);
            if byte != want {
                esp_println::println!(
                    "teddiebox: ota MISMATCH at {} — wrote {want:#04x}, read {byte:#04x}",
                    offset + index as u32
                );
                return;
            }
        }
        offset += CHUNK as u32;
    }

    esp_println::println!(
        "teddiebox: ota probe ok — {SECTOR} bytes in {CHUNK}-byte writes, {written} ms, read back clean"
    );
}

/// Arms the next boot on `slot`, in the state a freshly written image is left.
///
/// Spec §7a. [`OtaImageState::New`] is deliberate and is the whole experiment:
/// the bootloader promotes `New` to [`OtaImageState::PendingVerify`] only when
/// auto-rollback is enabled, so what [`status`] reads after the next boot says
/// which kind of bootloader this is — without installing anything broken.
///
/// **Both slots should hold a bootable image before this is used.** If
/// rollback turns out to be enabled and nothing marks the image valid, the
/// boot after next reverts to the other slot; if that slot is blank, the way
/// out is J100 with the case open. The command does not check, because it
/// cannot tell a valid image from a slot of erased flash without reading a
/// megabyte, and a check that is sometimes wrong is worse here than a stated
/// precondition.
pub fn arm_boot(slot: u8) {
    let target = match slot {
        0 => AppPartitionSubType::Ota0,
        _ => AppPartitionSubType::Ota1,
    };

    let flash = flash();
    let mut buffer = [0u8; PARTITION_TABLE_MAX_LEN];
    let table = match partitions::read_partition_table(flash, &mut buffer) {
        Ok(table) => table,
        Err(trouble) => {
            esp_println::println!("teddiebox: ota cannot read the partition table — {trouble:?}");
            return;
        }
    };

    let otadata = match table.find_partition(PartitionType::Data(DataPartitionSubType::Ota)) {
        Ok(Some(entry)) => entry,
        _ => {
            esp_println::println!("teddiebox: ota has no otadata partition — nothing to arm");
            return;
        }
    };

    let mut ota = match Ota::new(otadata.as_flash_region(flash), 2) {
        Ok(ota) => ota,
        Err(trouble) => {
            esp_println::println!("teddiebox: ota cannot open otadata — {trouble:?}");
            return;
        }
    };

    // A freshly repartitioned box has garbage here, not zeroes. `otadata` at
    // 0xd000 lands inside what the *old* layout called `nvs` (0x9000..0xf000),
    // and espflash writes only the bootloader, the table and the app — so the
    // sector still holds whatever Wi-Fi calibration was there. An erased slot
    // would validate fine; leftover NVS does not, and every call here fails
    // `Invalid` until something clears it.
    //
    // Resetting to `Factory` is the library's own way of saying "no slot is
    // selected": it rewrites both entries to 0xff through a path that erases
    // first. Done here rather than silently at boot because it throws away a
    // real selection if there ever is one, and the bench should see it happen.
    if ota.current_app_partition().is_err() {
        esp_println::println!("teddiebox: ota otadata is not initialised — clearing it first");
        if let Err(trouble) = ota.set_current_app_partition(AppPartitionSubType::Factory) {
            esp_println::println!("teddiebox: ota could not clear otadata — {trouble:?}");
            return;
        }
    }

    if let Err(trouble) = ota.set_current_app_partition(target) {
        esp_println::println!("teddiebox: ota could not select {target:?} — {trouble:?}");
        return;
    }
    if let Err(trouble) = ota.set_current_ota_state(OtaImageState::New) {
        esp_println::println!("teddiebox: ota could not set the state — {trouble:?}");
        return;
    }

    esp_println::println!(
        "teddiebox: ota armed {target:?} as New — reboot, then `otas`. \
         PendingVerify means this bootloader rolls back; New means it does not."
    );
}
