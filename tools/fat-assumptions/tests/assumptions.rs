//! Checks the assumptions about `embedded-sdmmc` that the download code
//! relies on, by running the library rather than reading its source.
//!
//! If one of these fails, the download design needs rethinking.

use embedded_sdmmc::{
    Error, Mode, RawDirectory, RawFile, TimeSource, Timestamp, VolumeIdx, VolumeManager,
};
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

/// Where the filesystem starts, in 512-byte blocks: 1 MiB, where card
/// formatters put the first partition.
const PARTITION_START_BLOCK: u32 = 2048;

/// `0x0E`, FAT16 with LBA addressing: one of the five types
/// `VolumeManager::open_raw_volume` is willing to mount.
const PARTITION_TYPE_FAT16_LBA: u8 = 0x0E;

/// A FAT-formatted image **in memory**, the same shape as the card's volume 0:
/// an MBR at block 0 naming one FAT16 partition, and that partition's
/// filesystem.
///
/// `embedded-sdmmc` can mount a FAT volume but cannot create one, so the test
/// formats its own (instead of committing an 8 MB image to git).
///
/// The partition table is needed: `open_raw_volume` reads block 0, requires
/// the `0xAA55` signature, and reads partition 0 from offset 446. A bare
/// filesystem also ends its first block with `0xAA55`, so it would be misread
/// as a garbage partition table rather than rejected.
///
/// **This only formats an in-memory `Vec<u8>`.** `format_volume` erases what
/// it is given, so never pass it a file or device.
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
        // FAT16 explicitly. `fatfs` would pick FAT16 for this size by itself,
        // but only by estimate; `embedded-sdmmc` cannot mount FAT12.
        fatfs::format_volume(
            &mut cursor,
            fatfs::FormatVolumeOptions::new().fat_type(fatfs::FatType::Fat16),
        )
        .expect("could not format the in-memory image");
    }

    write_master_boot_record(&mut image, PARTITION_START_BLOCK, partition_blocks);
    RamDisk::new(image)
}

/// The 66 bytes at the end of block 0 that `open_raw_volume` reads: one
/// partition entry and the signature. Everything else is zero.
fn write_master_boot_record(image: &mut [u8], start_block: u32, block_count: u32) {
    let entry = &mut image[446..462];
    entry[0] = 0x00; // not bootable, and not the 0x80 that means bootable
    entry[1..4].copy_from_slice(&[0xFE, 0xFF, 0xFF]); // CHS, not used
    entry[4] = PARTITION_TYPE_FAT16_LBA;
    entry[5..8].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
    entry[8..12].copy_from_slice(&start_block.to_le_bytes());
    entry[12..16].copy_from_slice(&block_count.to_le_bytes());
    image[510] = 0x55;
    image[511] = 0xAA;
}

/// `MAX_FILES` is **2**. With 1, the refusal in
/// `the_same_file_cannot_be_opened_twice` could be `TooManyOpenFiles`, and the
/// test would prove nothing.
type Volumes = VolumeManager<RamDisk, NoClock, 4, 2, 1>;

fn mounted(disk: RamDisk) -> Volumes {
    // `VolumeManager::new` only uses the default limits (4/4/1).
    VolumeManager::new_with_limits(disk, NoClock, 5000)
}

/// Mounts `disk` and opens its root directory, as the firmware does.
fn root_of(disk: RamDisk) -> (Volumes, RawDirectory) {
    let volumes = mounted(disk);
    let volume = volumes.open_raw_volume(VolumeIdx(0)).unwrap();
    let root = volumes.open_root_dir(volume).unwrap();
    (volumes, root)
}

/// Reads from `file` until `buf` is full, failing if the file ends first.
fn read_exactly(volumes: &Volumes, file: RawFile, buf: &mut [u8]) {
    let mut filled = 0;
    while filled < buf.len() {
        let n = volumes.read(file, &mut buf[filled..]).unwrap();
        assert_ne!(n, 0, "the file ended early");
        filled += n;
    }
}

/// What one read of up to 128 bytes returns, with the file closed after.
fn read_and_close(volumes: &Volumes, file: RawFile) -> Vec<u8> {
    let mut raw = [0u8; 128];
    let read = volumes.read(file, &mut raw).unwrap();
    volumes.close_file(file).unwrap();
    raw[..read].to_vec()
}

/// Writes into `disk` the way a laptop would, through `fatfs`, which can
/// create the long names and directories `embedded-sdmmc` cannot.
fn written_by_a_host(
    disk: RamDisk,
    write: impl FnOnce(&fatfs::Dir<std::io::Cursor<&mut [u8]>>),
) -> RamDisk {
    let mut image = disk.into_bytes();
    let partition_at = PARTITION_START_BLOCK as usize * 512;
    {
        let cursor = std::io::Cursor::new(&mut image[partition_at..]);
        let fs = fatfs::FileSystem::new(cursor, fatfs::FsOptions::new())
            .expect("could not mount the image as a host");
        write(&fs.root_dir());
    }
    RamDisk::new(image)
}

/// Creates `name` in `dir` holding `contents`, through `fatfs`.
fn host_file(dir: &fatfs::Dir<std::io::Cursor<&mut [u8]>>, name: &str, contents: &[u8]) {
    use std::io::Write;

    let mut file = dir.create_file(name).expect("could not create");
    file.write_all(contents).expect("could not write");
    file.flush().expect("could not flush");
}

/// The download uses one file handle for writing and reading: bytes written
/// at the end of a file can be read back from earlier in the file, on the
/// same handle, without closing it.
#[test]
fn one_handle_can_write_at_the_end_and_read_from_behind_it() {
    // Given: four 4096-byte pages, each filled with its own index
    let (volumes, root) = root_of(blank_fat_image_in_memory(8));
    let file = volumes
        .open_file_in_dir(root, "STORY.TAF", Mode::ReadWriteCreate)
        .unwrap();
    for page in 0u8..4 {
        volumes.file_seek_from_end(file, 0).unwrap();
        volumes.write(file, &[page; 4096]).unwrap();
    }
    volumes.flush_file(file).unwrap();

    // When: page 1 is read back from behind the write head, then one more
    // page is appended
    volumes.file_seek_from_start(file, 4096).unwrap();
    let mut page_1 = [0u8; 4096];
    read_exactly(&volumes, file, &mut page_1);
    volumes.file_seek_from_end(file, 0).unwrap();
    volumes.write(file, &[4u8; 4096]).unwrap();
    volumes.flush_file(file).unwrap();

    // Then
    assert!(page_1.iter().all(|&b| b == 1), "page 1 read back wrong");
    assert_eq!(volumes.file_length(file).unwrap(), 5 * 4096);
    volumes.close_file(file).unwrap();
}

/// Why `ContentSink::append` seeks to the end itself: after a read, the
/// offset is *in the middle*, and a write there silently overwrites instead
/// of appending.
#[test]
fn a_write_after_a_backward_seek_overwrites_unless_you_seek_to_the_end() {
    // Given: two pages, and the first read back, leaving the offset at 4096
    let (volumes, root) = root_of(blank_fat_image_in_memory(8));
    let file = volumes
        .open_file_in_dir(root, "STORY.TAF", Mode::ReadWriteCreate)
        .unwrap();
    volumes.write(file, &[1u8; 4096]).unwrap();
    volumes.write(file, &[2u8; 4096]).unwrap();
    volumes.flush_file(file).unwrap();
    assert_eq!(volumes.file_length(file).unwrap(), 8192);
    volumes.file_seek_from_start(file, 0).unwrap();
    read_exactly(&volumes, file, &mut [0u8; 4096]);

    // When
    volumes.write(file, &[9u8; 4096]).unwrap();
    volumes.flush_file(file).unwrap();

    // Then: the file did not grow, and page 1 now holds the new bytes
    assert_eq!(
        volumes.file_length(file).unwrap(),
        8192,
        "the file did not grow: the write overwrote page 1"
    );
    volumes.file_seek_from_start(file, 4096).unwrap();
    let mut page_1 = [0u8; 4096];
    read_exactly(&volumes, file, &mut page_1);
    assert!(
        page_1.iter().all(|&b| b == 9),
        "page 1 still held its old bytes, so nothing was overwritten"
    );
    volumes.close_file(file).unwrap();
}

/// Why a separate write handle and read handle on the same file are not
/// possible, whatever `MAX_FILES` is.
#[test]
fn the_same_file_cannot_be_opened_twice() {
    // Given
    let (volumes, root) = root_of(blank_fat_image_in_memory(8));
    let first = volumes
        .open_file_in_dir(root, "STORY.TAF", Mode::ReadWriteCreate)
        .unwrap();

    // When
    let refusal = volumes
        .open_file_in_dir(root, "STORY.TAF", Mode::ReadOnly)
        .expect_err("two handles on one file would make the ping-pong unnecessary");

    // Then: the exact error, since `is_err()` would also pass if the handle
    // limit (which this project sets) caused the refusal
    assert!(
        matches!(refusal, Error::FileAlreadyOpen),
        "the refusal must be about this file, not about how many handles fit: {refusal:?}"
    );
    volumes.close_file(first).unwrap();
}

/// A file's length on disk is only updated at a flush, so it lags behind
/// the open handle. This is why a separate sidecar records a download's
/// intended length.
///
/// This only shows across a remount: `close_file` also flushes. So the test
/// drops the `VolumeManager` with `free`, which releases the card without
/// closing the file, like a power loss.
#[test]
fn a_files_recorded_length_only_becomes_true_at_a_flush() {
    // Given: one page flushed, a second not, then the power lost
    let card = {
        let (volumes, root) = root_of(blank_fat_image_in_memory(8));
        let file = volumes
            .open_file_in_dir(root, "STORY.TAF", Mode::ReadWriteCreate)
            .unwrap();
        volumes.write(file, &[1u8; 4096]).unwrap();
        volumes.flush_file(file).unwrap();
        volumes.write(file, &[2u8; 4096]).unwrap();
        assert_eq!(
            volumes.file_length(file).unwrap(),
            8192,
            "the live handle counts every byte handed to it, flushed or not"
        );
        volumes.free().0
    };

    // When
    let (volumes, root) = root_of(card);
    let reopened = volumes
        .open_file_in_dir(root, "STORY.TAF", Mode::ReadOnly)
        .unwrap();
    let length = volumes.file_length(reopened).unwrap();

    // Then
    assert_eq!(
        length, 4096,
        "the on-disk record counts only what was flushed, and a resume reads it"
    );
    volumes.close_file(reopened).unwrap();
}

/// The config file is opened by name with `open_file_in_dir`, which only
/// takes short names. This does what the firmware does: create the file
/// under the crate's file name, read the bytes back, and parse them.
#[test]
fn the_config_file_can_be_opened_by_the_name_the_crate_publishes() {
    // Given
    const TEXT: &str = "ssid = HomeNet\npassword = hunter2\nserver = box.lan:80\n";
    let (volumes, root) = root_of(blank_fat_image_in_memory(8));
    let written = volumes
        .open_file_in_dir(root, teddiebox_config::FILENAME, Mode::ReadWriteCreate)
        .expect("the published name is not one this filesystem can open");
    volumes.write(written, TEXT.as_bytes()).unwrap();
    volumes.close_file(written).unwrap();

    // When
    let file = volumes
        .open_file_in_dir(root, teddiebox_config::FILENAME, Mode::ReadOnly)
        .unwrap();
    let raw = read_and_close(&volumes, file);
    let config = teddiebox_config::Config::parse(core::str::from_utf8(&raw).unwrap())
        .expect("the bytes that came off the filesystem did not parse");

    // Then
    assert_eq!(config.ssid.as_str(), "HomeNet");
    assert_eq!(config.server.as_str(), "box.lan:80");
}

/// What a long config file name such as `teddiebox.conf` would need.
///
/// It is possible: `ShortFileName` refuses a stem longer than eight
/// characters, but `open_long_name_file_in_dir` does open it. However, that
/// call scans every long name in the directory, cannot *create* a long-name
/// file, and would add a second way into the filesystem. A short name needs
/// none of that.
#[test]
fn a_long_config_filename_opens_only_through_the_long_name_call() {
    // Given: written by a laptop, since `open_long_name_file_in_dir` cannot
    // create files
    const LONG: &str = "teddiebox.conf";
    const TEXT: &str = "ssid = HomeNet\nserver = box.lan:80\n";
    let disk = written_by_a_host(blank_fat_image_in_memory(8), |root| {
        host_file(root, LONG, TEXT.as_bytes());
    });
    let (volumes, root) = root_of(disk);

    // When
    let refusal = volumes
        .open_file_in_dir(root, LONG, Mode::ReadOnly)
        .expect_err("a short-name open of a long name must fail");
    let file = volumes
        .open_long_name_file_in_dir(root, LONG, Mode::ReadOnly)
        .expect("a long name does open through the long-name call");
    let raw = read_and_close(&volumes, file);

    // Then: the exact error, since `is_err()` would also pass for a missing
    // file
    assert!(
        matches!(
            refusal,
            Error::FilenameError(embedded_sdmmc::FilenameError::NameTooLong)
        ),
        "the refusal must be about the ninth character of the stem: {refusal:?}"
    );
    assert_eq!(core::str::from_utf8(&raw).unwrap(), TEXT);
}

/// Whether a lower-case name written by a host is reachable by the box.
///
/// FAT stores an 8.3 name in upper case and marks "display in lower case" with
/// two flag bits, so `config.txt` and `CONFIG.TXT` should be the *same* short
/// entry, openable by the upper-case name `ShortFileName` produces. The laptop
/// writes the file and `embedded-sdmmc` reads it, so this checks both agree.
#[test]
fn a_lower_case_name_is_the_same_file_as_its_upper_case_short_name() {
    // Given: written by the host, in lower case, as on the real card
    const TEXT: &str = "ssid = HomeNet\nserver = box.lan:80\n";
    let disk = written_by_a_host(blank_fat_image_in_memory(8), |root| {
        host_file(root, "config.txt", TEXT.as_bytes());
    });
    let (volumes, root) = root_of(disk);

    // When
    let file = volumes
        .open_file_in_dir(root, "CONFIG.TXT", Mode::ReadOnly)
        .expect("a lower-case 8.3 name must be reachable by its short name");
    let raw = read_and_close(&volumes, file);

    // Then
    assert_eq!(core::str::from_utf8(&raw).unwrap(), TEXT);
}

/// `CardIndex` checks `CONTENT/` first and treats any error as "not on the
/// stock card", so a missing directory must fail cleanly.
#[test]
fn a_content_path_that_does_not_exist_fails_rather_than_panicking() {
    // Given
    let (volumes, root) = root_of(blank_fat_image_in_memory(8));

    // When
    let opened = volumes.open_dir(root, "CONTENT");

    // Then
    assert!(
        opened.is_err(),
        "a directory that was never created must not open"
    );
}

/// The two trees are independent: a downloaded story must not appear under
/// `CONTENT/`, and its sidecar must be readable back (otherwise a complete
/// file would be downloaded again forever).
#[test]
fn a_cached_story_and_its_sidecar_live_only_under_the_cache_tree() {
    // Given
    const BODY: &[u8] = &[0x00, 0x00, 0x0f, 0xfc];
    const SIDECAR: &[u8] = &[0xDE, 0xAD, 0xBE, 0xEF, 0x0A];
    let disk = written_by_a_host(blank_fat_image_in_memory(8), |root| {
        let dir = root
            .create_dir("CACHE")
            .unwrap()
            .create_dir("1C2D3E4F")
            .unwrap();
        host_file(&dir, "500304E0", BODY);
        host_file(&dir, "500304E0.MET", SIDECAR);
    });
    let (volumes, root) = root_of(disk);

    // When
    let stock_tree = volumes.open_dir(root, "CONTENT");
    let cache = volumes.open_dir(root, "CACHE").unwrap();
    let dir = volumes.open_dir(cache, "1C2D3E4F").unwrap();
    let body = volumes
        .open_file_in_dir(dir, "500304E0", Mode::ReadOnly)
        .expect("the cached story must open");
    let body_length = volumes.file_length(body).unwrap();
    volumes.close_file(body).unwrap();
    let met = volumes
        .open_file_in_dir(dir, "500304E0.MET", Mode::ReadOnly)
        .expect("the sidecar must open beside it");
    let sidecar = read_and_close(&volumes, met);

    // Then
    assert!(
        stock_tree.is_err(),
        "a cached story must not be reachable through the stock tree"
    );
    assert_eq!(body_length, BODY.len() as u32);
    assert_eq!(sidecar, SIDECAR);
}

/// The same for a directory, since the certificates are in one.
#[test]
fn a_lower_case_directory_is_reachable_by_its_short_name() {
    // Given
    let disk = written_by_a_host(blank_fat_image_in_memory(8), |root| {
        let dir = root.create_dir("cert").unwrap();
        host_file(&dir, "client.der", &[0x30, 0x82, 0x01, 0x02]);
    });
    let (volumes, root) = root_of(disk);

    // When
    let dir = volumes
        .open_dir(root, "CERT")
        .expect("a lower-case directory must be reachable by its short name");
    let file = volumes
        .open_file_in_dir(dir, "CLIENT.DER", Mode::ReadOnly)
        .expect("and so must a lower-case file inside it");
    let raw = read_and_close(&volumes, file);
    let _ = volumes.close_dir(dir);

    // Then
    assert_eq!(raw, [0x30, 0x82, 0x01, 0x02]);
}

/// Rewriting a file shorter than it was leaves no tail.
///
/// The config portal rewrites `CONFIG.TXT` in place (`embedded-sdmmc` 0.10
/// has no rename). If the old tail stayed, a shorter config would end with
/// pieces of the old one and might still parse.
#[test]
fn truncating_a_rewrite_leaves_no_tail() {
    // Given: a long config already on the card
    let (volumes, root) = root_of(blank_fat_image_in_memory(8));
    let long = b"ssid = averylongnetworkname\nserver = box.lan:443\n";
    let file = volumes
        .open_file_in_dir(root, "CONFIG.TXT", Mode::ReadWriteCreateOrTruncate)
        .unwrap();
    volumes.write(file, long).unwrap();
    volumes.flush_file(file).unwrap();
    volumes.close_file(file).unwrap();

    // When
    let short = b"ssid = x\n";
    let file = volumes
        .open_file_in_dir(root, "CONFIG.TXT", Mode::ReadWriteCreateOrTruncate)
        .unwrap();
    volumes.write(file, short).unwrap();
    volumes.flush_file(file).unwrap();
    let length = volumes.file_length(file).unwrap();
    volumes.close_file(file).unwrap();
    let file = volumes
        .open_file_in_dir(root, "CONFIG.TXT", Mode::ReadOnly)
        .unwrap();
    let raw = read_and_close(&volumes, file);

    // Then
    assert_eq!(
        length,
        short.len() as u32,
        "the directory entry still claims the old length"
    );
    assert_eq!(raw, short, "the previous config left a tail");
}
