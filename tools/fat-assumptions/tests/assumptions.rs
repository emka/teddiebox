//! Four things this project's download design rests on. Each was read out of
//! `embedded-sdmmc`'s source; these run it instead.
//!
//! If any of these fails, the design is
//! wrong, and it is much cheaper to learn that here than at the bench.

use embedded_sdmmc::{Error, Mode, TimeSource, Timestamp, VolumeIdx, VolumeManager};
use fat_assumptions::RamDisk;

struct NoClock;

impl TimeSource for NoClock {
    fn get_timestamp(&self) -> Timestamp {
        Timestamp {
            year_since_1970: 56,
            zero_indexed_month: 8,
            zero_indexed_day: 1,
            hours: 0,
            minutes: 0,
            seconds: 0,
        }
    }
}

/// Where the filesystem starts, in 512-byte blocks. One mebibyte in, which is
/// where every card formatter has put the first partition since 2009.
const PARTITION_START_BLOCK: u32 = 2048;

/// `0x0E`, FAT16 with LBA addressing: one of the five types
/// `VolumeManager::open_raw_volume` is willing to mount.
const PARTITION_TYPE_FAT16_LBA: u8 = 0x0E;

/// A FAT-formatted image **in memory**, the same shape as the card's volume 0:
/// an MBR at block 0 naming one FAT16 partition, and that partition's
/// filesystem.
///
/// `embedded-sdmmc` can mount a FAT volume but cannot create one, so a test
/// that wants a real filesystem has to bring its own. The alternative was
/// committing an eight-megabyte binary fixture to git.
///
/// The partition table is not decoration: `open_raw_volume` reads block 0,
/// insists on the `0xAA55` signature, and indexes `VolumeIdx(0)` into the
/// entry at offset 446. A bare filesystem also ends its first block in
/// `0xAA55`, so handing one over does not fail cleanly — it is misread as a
/// partition table full of rubbish.
///
/// **This formats a `Vec<u8>` and nothing else.** The bytes never leave the
/// process: no block device, no path, no file handle. `format_volume` is
/// destructive by nature, so never hand it anything but a fresh in-memory
/// buffer — passing it a `File` opened on a card or a `/dev/` node would
/// erase that device, and no test here has any reason to own one.
fn blank_fat_image_in_memory(megabytes: usize) -> RamDisk {
    let mut image = vec![0u8; megabytes * 1024 * 1024];
    let partition_at = PARTITION_START_BLOCK as usize * 512;
    assert!(
        partition_at < image.len(),
        "the image is smaller than the space reserved before the partition"
    );
    let partition_blocks = ((image.len() - partition_at) / 512) as u32;

    {
        let mut cursor = std::io::Cursor::new(&mut image[partition_at..]);
        // FAT16 explicitly. `fatfs` does pick FAT16 for this size on its own,
        // but by estimate: `determine_bytes_per_cluster` guesses a cluster
        // size and `determine_fs_geometry` then takes the first of FAT32,
        // FAT16, FAT12 that is self-consistent. Saying FAT16 here makes the
        // geometry a property of this test rather than of that estimator, and
        // `embedded-sdmmc` mounts FAT16 and FAT32 but not FAT12.
        fatfs::format_volume(
            &mut cursor,
            fatfs::FormatVolumeOptions::new().fat_type(fatfs::FatType::Fat16),
        )
        .expect("could not format the in-memory image");
    }

    write_master_boot_record(&mut image, PARTITION_START_BLOCK, partition_blocks);
    RamDisk::new(image)
}

/// The 66 bytes at the end of block 0 that `open_raw_volume` actually reads:
/// one partition entry and the signature. Everything else stays zero.
fn write_master_boot_record(image: &mut [u8], start_block: u32, block_count: u32) {
    let entry = &mut image[446..462];
    entry[0] = 0x00; // not bootable, and not the 0x80 that means bootable
    entry[1..4].copy_from_slice(&[0xFE, 0xFF, 0xFF]); // CHS, unread and unreadable
    entry[4] = PARTITION_TYPE_FAT16_LBA;
    entry[5..8].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
    entry[8..12].copy_from_slice(&start_block.to_le_bytes());
    entry[12..16].copy_from_slice(&block_count.to_le_bytes());
    image[510] = 0x55;
    image[511] = 0xAA;
}

/// `MAX_FILES` is deliberately **2**. With one, the refusal in
/// `the_same_file_cannot_be_opened_twice` could be `TooManyOpenFiles` and the
/// test would pass while proving nothing.
type Volumes = VolumeManager<RamDisk, NoClock, 4, 2, 1>;

fn mounted(disk: RamDisk) -> Volumes {
    // `VolumeManager::new` only builds the default 4/4/1 shape; the limits
    // this test wants have to go through `new_with_limits`.
    VolumeManager::new_with_limits(disk, NoClock, 5000)
}

/// The assumption the whole single-handle design rests on: bytes written at
/// the end of a file can be read back from behind the write head, on the same
/// handle, without closing it.
#[test]
fn one_handle_can_write_at_the_end_and_read_from_behind_it() {
    let volumes = mounted(blank_fat_image_in_memory(8));
    let volume = volumes.open_raw_volume(VolumeIdx(0)).unwrap();
    let root = volumes.open_root_dir(volume).unwrap();
    let file = volumes
        .open_file_in_dir(root, "STORY.TAF", Mode::ReadWriteCreate)
        .unwrap();

    // Write four 4096-byte pages, each filled with its own index.
    for page in 0u8..4 {
        volumes.file_seek_from_end(file, 0).unwrap();
        volumes.write(file, &[page; 4096]).unwrap();
    }
    volumes.flush_file(file).unwrap();

    // Read page 1 back from behind the write head.
    volumes.file_seek_from_start(file, 4096).unwrap();
    let mut buf = [0u8; 4096];
    let mut filled = 0;
    while filled < buf.len() {
        let n = volumes.read(file, &mut buf[filled..]).unwrap();
        assert_ne!(n, 0, "the file ended early");
        filled += n;
    }
    assert!(buf.iter().all(|&b| b == 1), "page 1 read back wrong");

    // And the write head is still where more bytes can be appended.
    volumes.file_seek_from_end(file, 0).unwrap();
    volumes.write(file, &[4u8; 4096]).unwrap();
    volumes.flush_file(file).unwrap();
    assert_eq!(volumes.file_length(file).unwrap(), 5 * 4096);

    volumes.close_file(file).unwrap();
}

/// Why `ContentSink::append` seeks to the end itself. After reading from
/// behind the write head the offset is *in the middle*, and a write there
/// overwrites rather than appends — silently, and in the part of the file the
/// decoder has already been promised.
#[test]
fn a_write_after_a_backward_seek_overwrites_unless_you_seek_to_the_end() {
    let volumes = mounted(blank_fat_image_in_memory(8));
    let volume = volumes.open_raw_volume(VolumeIdx(0)).unwrap();
    let root = volumes.open_root_dir(volume).unwrap();
    let file = volumes
        .open_file_in_dir(root, "STORY.TAF", Mode::ReadWriteCreate)
        .unwrap();

    volumes.write(file, &[1u8; 4096]).unwrap();
    volumes.write(file, &[2u8; 4096]).unwrap();
    volumes.flush_file(file).unwrap();
    assert_eq!(volumes.file_length(file).unwrap(), 8192);

    // Read the first page, leaving the offset at 4096.
    volumes.file_seek_from_start(file, 0).unwrap();
    let mut buf = [0u8; 4096];
    let mut filled = 0;
    while filled < buf.len() {
        filled += volumes.read(file, &mut buf[filled..]).unwrap();
    }

    // Writing now lands at 4096, not at the end.
    volumes.write(file, &[9u8; 4096]).unwrap();
    volumes.flush_file(file).unwrap();
    assert_eq!(
        volumes.file_length(file).unwrap(),
        8192,
        "the file did not grow: the write overwrote page 1"
    );

    // And it is page 1 specifically that is gone, not merely a byte count that
    // failed to move.
    volumes.file_seek_from_start(file, 4096).unwrap();
    let mut clobbered = [0u8; 4096];
    let mut filled = 0;
    while filled < clobbered.len() {
        let n = volumes.read(file, &mut clobbered[filled..]).unwrap();
        assert_ne!(n, 0, "the file ended early");
        filled += n;
    }
    assert!(
        clobbered.iter().all(|&b| b == 9),
        "page 1 still held its old bytes, so nothing was overwritten"
    );

    volumes.close_file(file).unwrap();
}

/// Why a write handle and a read handle on the same file are not an option,
/// whatever `MAX_FILES` is set to.
#[test]
fn the_same_file_cannot_be_opened_twice() {
    let volumes = mounted(blank_fat_image_in_memory(8));
    let volume = volumes.open_raw_volume(VolumeIdx(0)).unwrap();
    let root = volumes.open_root_dir(volume).unwrap();
    let first = volumes
        .open_file_in_dir(root, "STORY.TAF", Mode::ReadWriteCreate)
        .unwrap();

    let refusal = volumes
        .open_file_in_dir(root, "STORY.TAF", Mode::ReadOnly)
        .expect_err("two handles on one file would make the ping-pong unnecessary");
    // The variant matters. `is_err()` alone would still pass if the refusal
    // came from the handle budget, which is a limit this project chooses and
    // could raise, rather than from the library's rule about one file.
    assert!(
        matches!(refusal, Error::FileAlreadyOpen),
        "the refusal must be about this file, not about how many handles fit: {refusal:?}"
    );

    volumes.close_file(first).unwrap();
}

/// The sidecar exists because of this: a file's recorded length only becomes
/// true at a flush, so a download interrupted between flushes leaves a file
/// shorter than the bytes that reached the card. Resume reads the recorded
/// length, which is the conservative of the two.
///
/// The difference is only visible across a remount. `file_length` on a live
/// handle reads `FileInfo.entry.size`, which `write` updates immediately, and
/// `close_file` flushes on the way out — so a test that closes the file and
/// reopens it cannot tell a flush from a close, and would pass with the flush
/// deleted. This one drops the `VolumeManager` with `free`, which hands the
/// card back without closing anything, the way a flat battery would.
#[test]
fn a_files_recorded_length_only_becomes_true_at_a_flush() {
    let card = {
        let volumes = mounted(blank_fat_image_in_memory(8));
        let volume = volumes.open_raw_volume(VolumeIdx(0)).unwrap();
        let root = volumes.open_root_dir(volume).unwrap();
        let file = volumes
            .open_file_in_dir(root, "STORY.TAF", Mode::ReadWriteCreate)
            .unwrap();

        volumes.write(file, &[1u8; 4096]).unwrap();
        volumes.flush_file(file).unwrap();

        // A second page, and this time nothing tells the directory entry.
        volumes.write(file, &[2u8; 4096]).unwrap();
        assert_eq!(
            volumes.file_length(file).unwrap(),
            8192,
            "the live handle counts every byte handed to it, flushed or not"
        );

        // The power goes away here. `free` returns the card without closing
        // the file, so the unflushed length never reaches the directory.
        volumes.free().0
    };

    let volumes = mounted(card);
    let volume = volumes.open_raw_volume(VolumeIdx(0)).unwrap();
    let root = volumes.open_root_dir(volume).unwrap();
    let reopened = volumes
        .open_file_in_dir(root, "STORY.TAF", Mode::ReadOnly)
        .unwrap();
    assert_eq!(
        volumes.file_length(reopened).unwrap(),
        4096,
        "the on-disk record counts only what was flushed, and a resume reads it"
    );
    volumes.close_file(reopened).unwrap();
}
