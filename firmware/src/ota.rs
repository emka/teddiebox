//! The OTA parts that need the real hardware.
//!
//! The decisions are made in `teddiebox-ota`, where they are tested on the
//! host. This file:
//!
//! - confirms a new image once it has proved itself ([`mark_valid`]). The
//!   bootloader rolls back an image that resets before it is confirmed;
//! - provides console commands to inspect and test OTA on the box:
//!   [`status`] shows the booted slot and its state, [`write_probe`] tests a
//!   flash write into the spare slot (for example while Wi-Fi is connected),
//!   and [`arm_boot`] selects a slot for the next boot.

use embassy_time::Instant;
use esp_bootloader_esp_idf::ota::{Ota, OtaImageState};
use esp_bootloader_esp_idf::partitions::{
    self, AppPartitionSubType, DataPartitionSubType, PartitionType, PARTITION_TABLE_MAX_LEN,
};
use teddiebox_ota::{FlashRegionLike, Sectors, SECTOR};

/// How much of a sector each write carries.
///
/// Not a whole sector at once: a 4 KB stack buffer would use a quarter of the
/// stack headroom, and a real download also writes in smaller pieces.
///
/// A multiple of four, because `esp-storage` refuses writes whose offset or
/// length is not word-aligned, and uses a 4 KB *stack* buffer for slices that
/// are unaligned in memory.
const CHUNK: usize = 512;

/// The number of app partitions in `partitions.csv`, including `ota_2`, which
/// holds a stock image the firmware never writes.
///
/// The bootloader boots the slot `otadata` names modulo this count, so a
/// different number selects a different slot from the one asked for.
const APP_SLOTS: usize = 3;

/// The byte this probe expects at `offset` in the slot.
///
/// Depends on the offset, so a write in the wrong place fails the read-back.
fn expected(offset: u32) -> u8 {
    (offset as u8) ^ 0x5A
}

/// Opens `otadata` and returns the state of the running slot, if it is one of
/// ours.
///
/// `None` for a missing `otadata` partition or a selection that is not
/// `ota_0` or `ota_1` (`Factory`, the stock image in `ota_2`, or an unreadable
/// entry). A box that has never updated looks like this, and boots normally.
fn current_state() -> Option<OtaImageState> {
    let mut flash = crate::flash::flash();
    let mut buffer = [0u8; PARTITION_TABLE_MAX_LEN];
    let table = partitions::read_partition_table(&mut flash, &mut buffer).ok()?;
    let otadata = table
        .find_partition(PartitionType::Data(DataPartitionSubType::Ota))
        .ok()??;
    let mut ota = Ota::new(otadata.as_flash_region(&mut flash), APP_SLOTS).ok()?;

    match ota.current_app_partition().ok()? {
        AppPartitionSubType::Ota0 | AppPartitionSubType::Ota1 => {}
        _ => return None,
    }
    ota.current_ota_state().ok()
}

/// Opens `otadata`, printing exactly why if it could not.
///
/// For [`mark_valid`], which narrates every failure. [`current_state`] needs
/// no narration at all, so it does not use this.
fn open_otadata_verbose<'f>(
    flash: &'f mut crate::flash::Flash,
    buffer: &'f mut [u8; PARTITION_TABLE_MAX_LEN],
) -> Option<Ota<'f, 'static>> {
    let table = match partitions::read_partition_table(flash, buffer) {
        Ok(table) => table,
        Err(trouble) => {
            esp_println::println!("teddiebox: ota cannot read the partition table — {trouble:?}");
            return None;
        }
    };
    let otadata = match table.find_partition(PartitionType::Data(DataPartitionSubType::Ota)) {
        Ok(Some(entry)) => entry,
        Ok(None) => {
            esp_println::println!(
                "teddiebox: ota has no otadata partition — this flash cannot update"
            );
            return None;
        }
        Err(trouble) => {
            esp_println::println!("teddiebox: ota cannot find otadata — {trouble:?}");
            return None;
        }
    };
    match Ota::new(otadata.as_flash_region(flash), APP_SLOTS) {
        Ok(ota) => Some(ota),
        Err(trouble) => {
            esp_println::println!("teddiebox: ota cannot open otadata — {trouble:?}");
            None
        }
    }
}

/// Called once the card is mounted and the codec is up, which a broken build
/// would not reach. Only changes a slot that is still `PendingVerify`, so
/// extra calls, or calls on a box that never updated, do nothing.
pub fn mark_valid() {
    let Some(OtaImageState::PendingVerify) = current_state() else {
        return;
    };
    let mut flash = crate::flash::flash();
    let mut buffer = [0u8; PARTITION_TABLE_MAX_LEN];
    let Some(mut ota) = open_otadata_verbose(&mut flash, &mut buffer) else {
        return;
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
/// So the probe uses the same [`Sectors`] code as a real update.
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

/// The slot the running image is in, and the other one.
///
/// Returns the running slot's name and the other slot, which the probe
/// writes into.
fn slots(booted: AppPartitionSubType) -> (&'static str, AppPartitionSubType) {
    match booted {
        AppPartitionSubType::Ota1 => ("ota_1", AppPartitionSubType::Ota0),
        _ => ("ota_0", AppPartitionSubType::Ota1),
    }
}

/// Prints which slot booted and what `otadata` says about it.
///
/// Changes nothing. Shows whether a rollback happened or the box only
/// rebooted, and shows the result of [`arm_boot`] after a reboot.
pub fn status() {
    let mut flash = crate::flash::flash();
    let mut buffer = [0u8; PARTITION_TABLE_MAX_LEN];
    let table = match partitions::read_partition_table(&mut flash, &mut buffer) {
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

    let mut ota = match Ota::new(otadata.as_flash_region(&mut flash), APP_SLOTS) {
        Ok(ota) => ota,
        Err(trouble) => {
            esp_println::println!("teddiebox: ota cannot open otadata — {trouble:?}");
            return;
        }
    };

    // Print both even if one fails: on a box that has never updated, the
    // first succeeds and the second fails.
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
/// Tests whether a flash write works, for example with Wi-Fi off, connected
/// and idle, or during a download.
///
/// Writes to the slot that is *not* running, where a real update writes.
/// Finds the slot the probe should write into: not the one running.
///
/// Prints which slot it found and how big it is (or exactly why it could
/// not), then hands back the region to write through and its size — no
/// flash write happens before this returns.
fn locate_spare_slot<'f>(
    flash: &'f mut crate::flash::Flash,
    buffer: &'f mut [u8; PARTITION_TABLE_MAX_LEN],
) -> Option<(Region<'f, 'static>, u32)> {
    let table = match partitions::read_partition_table(flash, buffer) {
        Ok(table) => table,
        Err(trouble) => {
            esp_println::println!("teddiebox: ota cannot read the partition table — {trouble:?}");
            return None;
        }
    };

    let booted = match table.booted_partition() {
        Ok(Some(entry)) => entry,
        _ => {
            esp_println::println!("teddiebox: ota cannot tell which slot is running — not writing");
            return None;
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
            return None;
        }
        Err(trouble) => {
            esp_println::println!("teddiebox: ota cannot find the spare slot — {trouble:?}");
            return None;
        }
    };
    let slot_bytes = entry.len();
    let label = entry.label_as_str();
    esp_println::println!(
        "teddiebox: ota probing {label} ({slot_bytes} bytes) while running from {running_name}"
    );

    Some((Region(entry.as_flash_region(flash)), slot_bytes))
}

pub fn write_probe() {
    let mut flash = crate::flash::flash();
    let mut buffer = [0u8; PARTITION_TABLE_MAX_LEN];
    let Some((mut region, slot_bytes)) = locate_spare_slot(&mut flash, &mut buffer) else {
        return;
    };
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

    // Read back through the partition, since the adapter has no read.
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
/// Sets the slot to [`OtaImageState::New`], as an update does. A bootloader
/// with auto-rollback would change `New` to [`OtaImageState::PendingVerify`]
/// on the next boot, which [`status`] shows.
///
/// **Both slots should contain a bootable image before using this.** If the
/// boot fails and reverts to a blank slot, recovery needs J100 with the case
/// open. This is not checked, because telling a valid image from erased flash
/// would mean reading a megabyte.
pub fn arm_boot(slot: u8) {
    let target = match slot {
        0 => AppPartitionSubType::Ota0,
        _ => AppPartitionSubType::Ota1,
    };
    if select_next_boot(target) {
        esp_println::println!(
            "teddiebox: ota armed {target:?} as New — reboot, then `otas`. \
             The bootloader marks it PendingVerify, and `mark_valid` makes it Valid."
        );
    }
}

/// Points `otadata` at `target` in state [`OtaImageState::New`], so the next
/// boot runs it, as the first boot of a new image.
///
/// Prints exactly why and returns `false` if it could not.
pub fn select_next_boot(target: AppPartitionSubType) -> bool {
    let mut flash = crate::flash::flash();
    let mut buffer = [0u8; PARTITION_TABLE_MAX_LEN];
    let table = match partitions::read_partition_table(&mut flash, &mut buffer) {
        Ok(table) => table,
        Err(trouble) => {
            esp_println::println!("teddiebox: ota cannot read the partition table — {trouble:?}");
            return false;
        }
    };

    let otadata = match table.find_partition(PartitionType::Data(DataPartitionSubType::Ota)) {
        Ok(Some(entry)) => entry,
        _ => {
            esp_println::println!("teddiebox: ota has no otadata partition — nothing to arm");
            return false;
        }
    };

    let mut ota = match Ota::new(otadata.as_flash_region(&mut flash), APP_SLOTS) {
        Ok(ota) => ota,
        Err(trouble) => {
            esp_println::println!("teddiebox: ota cannot open otadata — {trouble:?}");
            return false;
        }
    };

    // `otadata` can hold anything but valid entries: a box that never selected
    // a slot has it blank, and one flashed over another layout can have other
    // data there. Every call here then fails with `Invalid` until it is cleared.
    //
    // Resetting to `Factory` means "no slot selected": it erases and rewrites
    // both entries. Done here, visibly, rather than silently at boot, because
    // it discards any real selection.
    if ota.current_app_partition().is_err() {
        esp_println::println!("teddiebox: ota otadata is not initialised — clearing it first");
        if let Err(trouble) = ota.set_current_app_partition(AppPartitionSubType::Factory) {
            esp_println::println!("teddiebox: ota could not clear otadata — {trouble:?}");
            return false;
        }
    }

    if let Err(trouble) = ota.set_current_app_partition(target) {
        esp_println::println!("teddiebox: ota could not select {target:?} — {trouble:?}");
        return false;
    }
    if let Err(trouble) = ota.set_current_ota_state(OtaImageState::New) {
        esp_println::println!("teddiebox: ota could not set the state — {trouble:?}");
        return false;
    }
    true
}

/// Where the slot that is not running lies in flash, so an update can be
/// written there without holding the flash handle between chunks.
pub struct SpareSlot {
    /// Absolute flash address of the slot's first byte.
    pub offset: u32,
    pub len: u32,
    /// What `otadata` calls the slot, for [`select_next_boot`].
    pub target: AppPartitionSubType,
}

/// Finds the slot the running image is not in.
///
/// `None`, after printing why, for a flash with no second app slot or a
/// partition table that cannot be read.
pub fn spare_slot() -> Option<SpareSlot> {
    let mut flash = crate::flash::flash();
    let mut buffer = [0u8; PARTITION_TABLE_MAX_LEN];
    let table = match partitions::read_partition_table(&mut flash, &mut buffer) {
        Ok(table) => table,
        Err(trouble) => {
            esp_println::println!("teddiebox: ota cannot read the partition table — {trouble:?}");
            return None;
        }
    };
    let running = match table.booted_partition() {
        Ok(Some(entry)) if entry.label_as_str() == "ota_1" => AppPartitionSubType::Ota1,
        Ok(Some(_)) => AppPartitionSubType::Ota0,
        _ => {
            esp_println::println!("teddiebox: ota cannot tell which slot is running");
            return None;
        }
    };
    let (_, target) = slots(running);
    match table.find_partition(PartitionType::App(target)) {
        Ok(Some(entry)) => Some(SpareSlot {
            offset: entry.offset(),
            len: entry.len(),
            target,
        }),
        _ => {
            esp_println::println!("teddiebox: ota has no second app slot to update into");
            None
        }
    }
}

/// Writes into a [`SpareSlot`] through the flash handle, at slot-relative
/// offsets.
///
/// Uses `write_nor`, which writes without erasing. `FlashStorage::write`,
/// which `partitions::FlashRegion` uses, reads, erases and rewrites the
/// whole 4 KB sector on every call: eight erases per sector for 512-byte
/// writes. [`Sectors`] already erases each sector once before its first
/// write.
pub struct SlotWriter<'f> {
    pub flash: &'f mut crate::flash::Flash,
    pub offset: u32,
}

impl FlashRegionLike for SlotWriter<'_> {
    type Error = esp_storage::FlashStorageError;

    fn erase(&mut self, range: core::ops::Range<u32>) -> Result<(), Self::Error> {
        self.flash
            .erase(self.offset + range.start, self.offset + range.end)
    }

    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        self.flash.write_nor(self.offset + offset, bytes)
    }
}
