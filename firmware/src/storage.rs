//! The SD card, in SPI mode.
//!
//! Reads stories under `CONTENT/` and writes downloaded audio there too, in
//! the same layout, so a story the stock firmware fetched is not fetched
//! again. A download's sidecar and saved position live under `CACHE/`, and
//! those of stock stories next to them under `CONTENT/` (`.POS`). Also writes
//! `CONFIG.TXT`.
//!
//! The `sd` walk checksums every file on the card, to compare with the same
//! files read on a laptop by `scripts/sd-checksums.py`, which prints the same
//! columns so the two can be compared with `diff`. The checksum is
//! [`teddiebox_core::checksum`] and the traversal order
//! [`teddiebox_core::walk`], both tested on the host.
//!
//! **The walk yields** to the executor, so the console and other tasks keep
//! working during a walk that can take hours. It uses a cursor rather than
//! recursion, because recursive async needs boxed futures and this firmware
//! does not allocate (the only heap belongs to the Wi-Fi driver).
//!
//! **Names are 8.3 short names**, as stored in the directory entries. A file
//! with a long name shows its short alias here and its long name on a laptop,
//! so the paths differ even though the bytes match. A Toniebox card has no
//! long names.

use core::fmt::Write as _;
use core::ops::ControlFlow;
use portable_atomic::{AtomicU64, Ordering};

use embassy_futures::yield_now;
use embassy_time::Instant;
use embedded_hal_bus::spi::ExclusiveDevice;
use teddiebox_config::{Config, ConfigError};

use embedded_sdmmc::{
    Mode, RawDirectory, RawFile, SdCard, ShortFileName, TimeSource, Timestamp, VolumeIdx,
    VolumeManager,
};
use esp_hal::delay::Delay;
use esp_hal::gpio::Output;
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::Rate;
use teddiebox_core::checksum::Crc32;
use teddiebox_core::walk::{Action, Cursor, Found, MAX_DEPTH};
use teddiebox_taf::{PageSource, PAGE_SIZE};

/// Card initialisation runs at 400 kHz, which the SD specification requires
/// until the card has been identified.
const INIT_RATE_KHZ: u32 = 400;

/// The rate the walk runs at once the card is up.
///
/// Conservative on purpose: correct bytes matter more than speed, and errors
/// from clocking a card too fast would look like filesystem faults.
const WALK_RATE_KHZ: u32 = 8_000;

/// How often the file read hands the executor back.
///
/// Every 16 blocks (8 KiB), about 30 ms at this bus speed: often enough for
/// other tasks and the console, rarely enough not to slow the walk.
const YIELD_EVERY_BLOCKS: u32 = 16;

/// Microseconds spent inside [`CardPages::read_page`].
///
/// Playback measures decode time including the card reads underneath
/// `next_frame`. This counts the card reads separately, to see which one
/// takes longer.
///
/// 64-bit via `portable-atomic`, because Xtensa has no native 64-bit atomic
/// and a `u32` of microseconds wraps after about 70 minutes, less than some
/// Tonies.
pub static PAGE_READ_US: AtomicU64 = AtomicU64::new(0);

/// The longest single page read, in microseconds.
///
/// The total shows how much time the card takes overall; this shows whether
/// any *single* read was long enough to matter. The audio buffer holds about
/// 170 ms, so one read longer than that starves the DMA.
pub static PAGE_READ_MAX_US: AtomicU64 = AtomicU64::new(0);

/// Where a Toniebox keeps its audio, and what it calls it.
///
/// Fixed by the Toniebox's layout: every story on a real card is at
/// `CONTENT/<8 hex>/500304E0`.
const TONIE_CONTENT_DIR: &str = "CONTENT";

/// Where a download keeps everything but its audio: the sidecar that says how
/// long the audio should be, and the saved position.
///
/// The same `<8 hex>/<8 hex>` layout as `CONTENT`. The stock firmware does not
/// read it.
const CACHE_DIR: &str = "CACHE";

/// Where the card's copy of the certificate authority lives.
///
/// Holds only `TCCA.DER`; the box's own certificate and key are in the
/// `cert` flash partition. Already an 8.3 name.
const CERT_DIR: &str = "CERT";

/// Why a certificate could not be read off the card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertificateError {
    /// No `CERT` directory, or no file of that name in it.
    Missing,
    /// Larger than the buffer, so not one the box can use.
    TooLarge,
    /// The card would not give it back.
    Unreadable,
}

impl CertificateError {
    /// Says what went wrong, for the console.
    pub fn why(self) -> &'static str {
        match self {
            CertificateError::Missing => "no CERT/TCCA.DER on the card",
            CertificateError::TooLarge => "the certificate is larger than its buffer",
            CertificateError::Unreadable => "the certificate would not read",
        }
    }
}

/// Why the card's configuration could not be used.
///
/// Three cases with different meanings: no file (normal on a new card), a
/// file that cannot be read (card or wiring fault), and a file that does not
/// parse (a typo).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigTrouble {
    /// No such file in the card's root.
    Missing,
    /// The file is there but the card would not give it up.
    Unreadable,
    /// The file read, and is not a configuration.
    Refused(ConfigError),
}

const TONIE_AUDIO_FILE: &str = "500304E0";

/// Longest path the walk will print.
const MAX_PATH: usize = 64;

/// FAT needs a timestamp for new files. The box has no clock, so every
/// timestamp is zero.
struct NoClock;

impl TimeSource for NoClock {
    fn get_timestamp(&self) -> Timestamp {
        Timestamp {
            year_since_1970: 0,
            zero_indexed_month: 0,
            zero_indexed_day: 0,
            hours: 0,
            minutes: 0,
            seconds: 0,
        }
    }
}

/// The path of the directory currently being walked.
///
/// One buffer for the whole walk, extended and shortened as it goes, so the
/// memory used does not grow with depth.
struct Path {
    buf: [u8; MAX_PATH],
    len: usize,
}

impl Path {
    const fn new() -> Self {
        Self {
            buf: [0; MAX_PATH],
            len: 0,
        }
    }

    /// Appends `/name`, and reports where to truncate back to.
    ///
    /// Stops at the buffer's end: a cut-off path shows up in the `diff` with
    /// the host.
    fn push(&mut self, name: &ShortFileName) -> usize {
        let mark = self.len;
        let mut writer = PathWriter { path: self };
        let _ = write!(writer, "/{name}");
        mark
    }

    fn truncate(&mut self, mark: usize) {
        self.len = mark;
    }

    fn as_str(&self) -> &str {
        // Every byte came from a ShortFileName, which is 8.3 ASCII.
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("<unprintable>")
    }
}

struct PathWriter<'a> {
    path: &'a mut Path,
}

impl core::fmt::Write for PathWriter<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for &byte in s.as_bytes() {
            if self.path.len == MAX_PATH {
                break;
            }
            self.path.buf[self.path.len] = byte;
            self.path.len += 1;
        }
        Ok(())
    }
}

/// Page-indexed reads of one file on the card.
///
/// `teddiebox-taf` reads 4096-byte pages, because every structure in a TAF
/// file is 4096-aligned. Here that is a seek and a read.
pub struct CardPages<'a> {
    card: &'a Mounted,
    file: RawFile,
    pages: u32,
}

impl<'a> CardPages<'a> {
    /// Wraps an open file. `size` is its length in bytes.
    pub fn new(card: &'a Mounted, file: RawFile, size: u32) -> Self {
        Self {
            card,
            file,
            // Round up: the last page of a file is often short.
            pages: size.div_ceil(PAGE_SIZE as u32),
        }
    }
}

impl PageSource for CardPages<'_> {
    type Error = &'static str;

    fn read_page(&mut self, index: u32, buf: &mut [u8; PAGE_SIZE]) -> Result<(), Self::Error> {
        let began = Instant::now();
        let result = self.read_page_inner(index, buf);
        let took = began.elapsed().as_micros();
        PAGE_READ_US.fetch_add(took, Ordering::Relaxed);
        PAGE_READ_MAX_US.fetch_max(took, Ordering::Relaxed);
        result
    }

    fn page_count(&self) -> u32 {
        self.pages
    }
}

impl CardPages<'_> {
    fn read_page_inner(
        &mut self,
        index: u32,
        buf: &mut [u8; PAGE_SIZE],
    ) -> Result<(), &'static str> {
        let offset = index
            .checked_mul(PAGE_SIZE as u32)
            .ok_or("page index out of range")?;
        self.card.seek(self.file, offset)?;

        // One read may not fill the buffer, so repeat until full or the end
        // of the file.
        let mut filled = 0;
        while filled < PAGE_SIZE {
            let read = self.card.read(self.file, &mut buf[filled..])?;
            if read == 0 {
                break;
            }
            filled += read;
        }

        // Zero-fill a short last page. Safe because an Ogg page states its own
        // length.
        buf[filled..].fill(0);
        Ok(())
    }
}

/// One thing found in a directory.
#[derive(Clone, Copy)]
struct Entry {
    name: ShortFileName,
    size: u32,
    is_dir: bool,
}

type Card = SdCard<ExclusiveDevice<Spi<'static, esp_hal::Blocking>, Output<'static>, Delay>, Delay>;

/// The volume manager, with limits matching what is open at once.
///
/// The directory limit is [`MAX_DEPTH`]: the walk keeps every directory on
/// the current path open. A separate number could fall below the depth
/// limit, and the walk would fail with `TooManyOpenDirs` on a deep card.
///
/// **Two files**: a download keeps its cache file open, and the box must
/// still be able to open another file (a story or a system sound) meanwhile.
/// Reading and writing the *same* file shares one handle, because
/// `embedded-sdmmc` cannot open one file twice.
///
/// One volume.
type Volumes = VolumeManager<Card, NoClock, MAX_DEPTH, 2, 1>;

/// A mounted card, and the handles that keep it open.
///
/// Never closed: once mounted, the card stays mounted while the box runs.
/// Its power rail is shared with the NFC reader, so it is not turned off.
pub struct Mounted {
    volumes: Volumes,
    root: RawDirectory,
}

impl Mounted {
    /// Brings the card up and mounts partition 0.
    ///
    /// The storage rail must already be on. The caller turns it on, since the
    /// rail is shared with the NFC reader.
    pub fn open(
        spi: Spi<'static, esp_hal::Blocking>,
        cs: Output<'static>,
        delay: Delay,
    ) -> Result<Self, &'static str> {
        let device =
            ExclusiveDevice::new(spi, cs, delay).map_err(|_| "chip select would not drive")?;
        let card = SdCard::new(device, delay);

        // The first transaction identifies the card, so a dead bus shows up
        // here.
        let bytes = card.num_bytes().map_err(|_| "no card answered")?;
        esp_println::println!(
            "teddiebox: card {} MiB, type {:?}",
            bytes / (1024 * 1024),
            card.get_card_type()
        );

        // The card is identified, so the bus can run faster now.
        card.spi(|device| {
            let faster = SpiConfig::default().with_frequency(Rate::from_khz(WALK_RATE_KHZ));
            if device.bus_mut().apply_config(&faster).is_err() {
                esp_println::println!("teddiebox: card stays at {INIT_RATE_KHZ} kHz");
            }
        });

        let volumes: Volumes = VolumeManager::new_with_limits(card, NoClock, 5000);
        let volume = volumes
            .open_raw_volume(VolumeIdx(0))
            .map_err(|_| "no FAT volume in partition 0")?;
        let root = volumes
            .open_root_dir(volume)
            .map_err(|_| "no root directory")?;

        Ok(Self { volumes, root })
    }

    /// Checksums every file on the card.
    pub async fn walk(&self) {
        esp_println::println!("teddiebox: sd walk begins");
        walk_tree(&self.volumes, self.root).await;
        esp_println::println!("teddiebox: sd walk done");
    }

    /// The first file in the root directory with this extension.
    ///
    /// Only the root: test files are copied there, and searching the whole
    /// card would mean reading every directory.
    pub fn find_by_extension(&self, extension: &[u8]) -> Option<(ShortFileName, u32)> {
        let mut index = 0;
        while let Some(entry) = nth_entry(&self.volumes, self.root, index) {
            index += 1;
            if !entry.is_dir && entry.name.extension() == extension {
                return Some((entry.name, entry.size));
            }
        }
        None
    }

    /// Opens the first Tonie audio file on the card.
    ///
    /// A real card keeps stories at `CONTENT/<8 hex>/500304E0`, with no file
    /// extension, so the root search above does not find them.
    ///
    /// Read-only, so it works with the card's original content.
    ///
    /// The directories are closed before returning: an open file does not
    /// need its parent directory open.
    pub fn open_first_tonie(&self) -> Option<(RawFile, u32)> {
        let content = self.volumes.open_dir(self.root, TONIE_CONTENT_DIR).ok()?;

        let mut index = 0;
        let found = loop {
            let Some(entry) = nth_entry(&self.volumes, content, index) else {
                break None;
            };
            index += 1;
            if !entry.is_dir {
                continue;
            }

            let Ok(folder) = self.volumes.open_dir(content, entry.name) else {
                continue;
            };
            let opened = self
                .volumes
                .open_file_in_dir(folder, TONIE_AUDIO_FILE, Mode::ReadOnly)
                .ok()
                .map(|file| (file, entry.name));
            let size = opened.as_ref().and_then(|_| {
                let mut at = 0;
                loop {
                    let e = nth_entry(&self.volumes, folder, at)?;
                    at += 1;
                    if !e.is_dir && e.name.extension().is_empty() {
                        break Some(e.size);
                    }
                }
            });
            let _ = self.volumes.close_dir(folder);

            if let (Some((file, folder_name)), Some(size)) = (opened, size) {
                esp_println::println!("teddiebox: tonie /CONTENT/{folder_name}/500304E0");
                break Some((file, size));
            }
        };

        let _ = self.volumes.close_dir(content);
        found
    }

    /// Opens `CONTENT/<directory>/<file>`, the path the box keeps audio under.
    pub fn open_content(&self, directory: u32, file: u32) -> Result<(RawFile, u32), &'static str> {
        self.open_audio_under(TONIE_CONTENT_DIR, directory, file)
    }

    /// Reads `CONTENT/<directory>/<file>` end to end and returns its size and
    /// CRC32, without walking the rest of the card.
    ///
    /// The same [`Crc32`] and loop as the walk uses, for one file, so a
    /// download can be checked without a walk that takes hours.
    pub async fn checksum_audio(
        &self,
        directory: u32,
        file: u32,
    ) -> Result<(u32, u32), &'static str> {
        let (raw_file, size) = self.open_content(directory, file)?;

        let mut crc = Crc32::new();
        let mut buffer = [0u8; 512];
        let mut read: u64 = 0;
        let mut blocks: u32 = 0;

        loop {
            match self.read(raw_file, &mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    crc.update(&buffer[..n]);
                    read += n as u64;
                }
                Err(reason) => {
                    self.close_file(raw_file);
                    return Err(reason);
                }
            }

            blocks += 1;
            if blocks.is_multiple_of(YIELD_EVERY_BLOCKS) {
                yield_now().await;
            }
        }

        self.close_file(raw_file);

        // Report a short read, rather than a checksum of part of the file.
        if read != u64::from(size) {
            return Err("short read");
        }

        Ok((size, crc.finish()))
    }

    /// Opens `<tree>/<directory>/<file>` for reading, and says how long it is.
    ///
    /// Names are formatted here as eight upper-case hex digits, as FAT stores
    /// them, so a caller cannot pass a wrongly formatted name.
    fn open_audio_under(
        &self,
        tree: &str,
        directory: u32,
        file: u32,
    ) -> Result<(RawFile, u32), &'static str> {
        let folder_name = teddiebox_download::hex8(directory);
        let file_name = teddiebox_download::hex8(file);
        let folder_name =
            core::str::from_utf8(&folder_name).map_err(|_| "the directory name is not text")?;
        let file_name =
            core::str::from_utf8(&file_name).map_err(|_| "the file name is not text")?;

        let content = self
            .volumes
            .open_dir(self.root, tree)
            .map_err(|_| "no such tree on the card")?;
        let folder = match self.volumes.open_dir(content, folder_name) {
            Ok(folder) => folder,
            Err(_) => {
                let _ = self.volumes.close_dir(content);
                return Err("no such directory in that tree");
            }
        };

        // The size comes from the directory entry: the decoder needs it
        // before reading.
        let mut size = None;
        let mut at = 0;
        while let Some(entry) = nth_entry(&self.volumes, folder, at) {
            at += 1;
            if !entry.is_dir && entry.name.base_name() == file_name.as_bytes() {
                size = Some(entry.size);
                break;
            }
        }

        let opened = self
            .volumes
            .open_file_in_dir(folder, file_name, Mode::ReadOnly)
            .ok();
        let _ = self.volumes.close_dir(folder);
        let _ = self.volumes.close_dir(content);

        match (opened, size) {
            (Some(handle), Some(size)) => {
                esp_println::println!(
                    "teddiebox: content /{tree}/{folder_name}/{file_name}, {size} bytes"
                );
                Ok((handle, size))
            }
            (Some(handle), None) => {
                self.close_file(handle);
                Err("the file is there but has no directory entry")
            }
            (None, _) => Err("no such content file"),
        }
    }

    /// Reads and parses the card's configuration file.
    ///
    /// Only the media task uses the card, so this returns a parsed value, not
    /// a file handle.
    ///
    /// `buffer` comes from the caller, to avoid a permanent buffer for a
    /// one-time read. Its size is the file-size limit: a read that fills it is
    /// refused, since a cut-off config could still parse.
    pub fn read_config(&self, buffer: &mut [u8]) -> Result<Config, ConfigTrouble> {
        let name = ShortFileName::create_from_str(teddiebox_config::FILENAME)
            .map_err(|_| ConfigTrouble::Missing)?;
        let file = self.open_file(name).map_err(|_| ConfigTrouble::Missing)?;

        let mut filled = 0;
        let outcome = loop {
            if filled == buffer.len() {
                // Full: the file may be longer.
                break Err(ConfigTrouble::Refused(ConfigError::Truncated));
            }
            match self.read(file, &mut buffer[filled..]) {
                Ok(0) => break Ok(filled),
                Ok(n) => filled += n,
                Err(_) => break Err(ConfigTrouble::Unreadable),
            }
        };
        self.close_file(file);

        let filled = outcome?;
        Config::parse_read(&buffer[..filled], buffer.len()).map_err(ConfigTrouble::Refused)
    }

    /// The config file's bytes, exactly as the card holds them.
    ///
    /// Unlike [`Mounted::read_config`], this returns the raw bytes, for the
    /// portal to show for editing, comments included.
    ///
    /// A missing file returns `Ok(0)`: a new box, not an error.
    pub fn read_config_bytes(&self, buffer: &mut [u8]) -> Result<usize, &'static str> {
        let name = ShortFileName::create_from_str(teddiebox_config::FILENAME)
            .map_err(|_| "the config name is not a short name")?;
        let Ok(file) = self.open_file(name) else {
            return Ok(0);
        };

        let mut filled = 0;
        let outcome = loop {
            if filled == buffer.len() {
                // Full does not mean too long: the box can write a file of
                // exactly `buffer.len()` bytes. Try to read one more byte; if
                // there is none, the file fits.
                let mut past = [0u8; 1];
                break match self.read(file, &mut past) {
                    Ok(0) => Ok(filled),
                    Ok(_) => Err("the config file is larger than the buffer"),
                    Err(_) => Err("the config file would not read"),
                };
            }
            match self.read(file, &mut buffer[filled..]) {
                Ok(0) => break Ok(filled),
                Ok(n) => filled += n,
                Err(_) => break Err("the config file would not read"),
            }
        };
        self.close_file(file);
        outcome
    }

    /// Replaces the config file.
    ///
    /// **Not atomic**: `embedded-sdmmc` 0.10 cannot rename, so the file cannot
    /// be written elsewhere and swapped in. A power cut during the write
    /// leaves a short file, and the box will not connect to Wi-Fi.
    ///
    /// Setup mode (both ears held at boot) can still fix that, because its
    /// credentials are built in. `tools/fat-assumptions` checks that the
    /// truncation leaves no old tail.
    pub fn write_config(&self, bytes: &[u8]) -> Result<(), &'static str> {
        let name = ShortFileName::create_from_str(teddiebox_config::FILENAME)
            .map_err(|_| "the config name is not a short name")?;
        let file = self
            .volumes
            .open_file_in_dir(self.root, name, Mode::ReadWriteCreateOrTruncate)
            .map_err(|_| "the config file would not open for writing")?;

        let outcome = self
            .volumes
            .write(file, bytes)
            .map_err(|_| "the config file would not write")
            .and_then(|()| {
                self.volumes
                    .flush_file(file)
                    .map_err(|_| "the config file would not flush")
            });
        self.close_file(file);
        outcome
    }

    /// Opens `/CONTENT/<directory>/<file>` for writing, creating whatever is
    /// missing, and **discards anything already there**.
    ///
    /// For a download that starts from the beginning. To continue one, use
    /// [`Mounted::open_download_for_append`].
    ///
    /// **One handle, opened once.** The reader and writer share it (the
    /// library cannot open a file twice), so the offset moves between reads
    /// and appends; this is why [`Mounted::append`] seeks to the end itself.
    pub fn open_download_for_write(
        &self,
        directory: u32,
        file: u32,
    ) -> Result<RawFile, &'static str> {
        self.open_download_in_mode(directory, file, Mode::ReadWriteCreateOrTruncate)
    }

    /// Opens the same file **keeping what is already in it**, to continue an
    /// interrupted download.
    ///
    /// Only correct when a sidecar confirms where the file ends and the
    /// server agreed to continue from there; [`teddiebox_download::place`]
    /// decides that. Otherwise the start of a story would be appended to its
    /// middle, at the right length, so nothing later would notice.
    pub fn open_download_for_append(
        &self,
        directory: u32,
        file: u32,
    ) -> Result<RawFile, &'static str> {
        self.open_download_in_mode(directory, file, Mode::ReadWriteCreateOrAppend)
    }

    fn open_download_in_mode(
        &self,
        directory: u32,
        file: u32,
        mode: Mode,
    ) -> Result<RawFile, &'static str> {
        let folder_name = teddiebox_download::hex8(directory);
        let file_name = teddiebox_download::hex8(file);
        let folder_name =
            core::str::from_utf8(&folder_name).map_err(|_| "the directory name is not text")?;
        let file_name =
            core::str::from_utf8(&file_name).map_err(|_| "the file name is not text")?;

        // Close every handle before returning, including on errors. Only
        // MAX_DEPTH directory handles exist (shared with the walk), and
        // leaking them would make the next download fail as if the card were
        // unwritable.
        let content = self.open_or_make_dir(self.root, TONIE_CONTENT_DIR)?;
        let folder = self.open_or_make_dir(content, folder_name);
        let _ = self.volumes.close_dir(content);
        let folder = folder?;

        let handle = self.volumes.open_file_in_dir(folder, file_name, mode);
        let _ = self.volumes.close_dir(folder);
        handle.map_err(|_| "the download file would not open")
    }

    /// How long the audio file under `CONTENT/` is, or `None` if there is
    /// not one.
    ///
    /// Opens and closes the file here, because the library cannot open a file
    /// twice, so this must happen before the download opens it.
    pub fn audio_length(&self, directory: u32, file: u32) -> Option<u32> {
        let (handle, length) = self.open_content(directory, file).ok()?;
        self.close_file(handle);
        Some(length)
    }

    /// Whether `/CACHE/<directory>/<file>.MET` exists, readable or not.
    ///
    /// A sidecar that exists but cannot be read still marks the story as a
    /// download, so it is fetched again rather than taken for stock content.
    pub fn has_sidecar(&self, directory: u32, file: u32) -> bool {
        match self.open_sidecar(directory, file) {
            Some(handle) => {
                self.close_file(handle);
                true
            }
            None => false,
        }
    }

    /// Reads `/CACHE/<directory>/<file>.MET` into `buffer`.
    ///
    /// Returns how many bytes it held. Every failure means "no sidecar": one
    /// that cannot be read proves nothing, so the content counts as
    /// incomplete.
    pub fn read_sidecar(&self, directory: u32, file: u32, buffer: &mut [u8]) -> Option<usize> {
        let handle = self.open_sidecar(directory, file)?;
        let filled = self.volumes.read(handle, buffer).ok();
        self.close_file(handle);
        filled
    }

    /// Opens `/CACHE/<directory>/<file>.MET` for reading.
    fn open_sidecar(&self, directory: u32, file: u32) -> Option<RawFile> {
        let folder_name = teddiebox_download::hex8(directory);
        let stem = teddiebox_download::hex8(file);
        let folder_name = core::str::from_utf8(&folder_name).ok()?;

        let mut file_name = [0u8; 12];
        file_name[..8].copy_from_slice(&stem);
        file_name[8..].copy_from_slice(b".MET");
        let file_name = core::str::from_utf8(&file_name).ok()?;

        // Create nothing here: reading should not create directories.
        let cache = self.volumes.open_dir(self.root, CACHE_DIR).ok()?;
        let folder = match self.volumes.open_dir(cache, folder_name) {
            Ok(folder) => folder,
            Err(_) => {
                let _ = self.volumes.close_dir(cache);
                return None;
            }
        };
        let _ = self.volumes.close_dir(cache);

        let handle = self
            .volumes
            .open_file_in_dir(folder, file_name, Mode::ReadOnly);
        let _ = self.volumes.close_dir(folder);
        handle.ok()
    }

    /// Writes `/CACHE/<directory>/<file>.MET`, the record that says how long
    /// the content beside it is meant to be.
    ///
    /// **Written before the first body byte, and closed before returning**,
    /// so it survives a power loss during the download. Audio under `CONTENT/`
    /// without a sidecar counts as stock content and complete, so a download
    /// whose sidecar cannot be written must not go on.
    pub fn write_sidecar(
        &self,
        directory: u32,
        file: u32,
        bytes: &[u8],
    ) -> Result<(), &'static str> {
        let folder_name = teddiebox_download::hex8(directory);
        let stem = teddiebox_download::hex8(file);
        let folder_name =
            core::str::from_utf8(&folder_name).map_err(|_| "the directory name is not text")?;

        // `<8 hex>.MET`, a valid 8.3 name.
        let mut file_name = [0u8; 12];
        file_name[..8].copy_from_slice(&stem);
        file_name[8..].copy_from_slice(b".MET");
        let file_name =
            core::str::from_utf8(&file_name).map_err(|_| "the sidecar name is not text")?;

        // As in `open_cache_in_mode`: every path closes both directory
        // handles before returning.
        let cache = self.open_or_make_dir(self.root, CACHE_DIR)?;
        let folder = self.open_or_make_dir(cache, folder_name);
        let _ = self.volumes.close_dir(cache);
        let folder = folder?;

        let handle =
            self.volumes
                .open_file_in_dir(folder, file_name, Mode::ReadWriteCreateOrTruncate);
        let _ = self.volumes.close_dir(folder);
        let handle = handle.map_err(|_| "the sidecar would not open")?;

        let wrote = self
            .volumes
            .write(handle, bytes)
            .map_err(|_| "the sidecar write failed")
            .and_then(|()| {
                self.volumes
                    .flush_file(handle)
                    .map_err(|_| "the sidecar flush failed")
            });
        self.close_file(handle);
        wrote
    }

    /// Reads `/CONTENT|CACHE/<directory>/<file>.POS`, the page a story should
    /// resume at.
    ///
    /// Separate from the `.MET` sidecar, which `decide()` uses to tell a
    /// complete download from a partial one. An interrupted write of the
    /// often-changing position must not damage that.
    ///
    /// Creates nothing, like `read_sidecar`.
    pub fn read_position(
        &self,
        stock: bool,
        directory: u32,
        file: u32,
        buffer: &mut [u8],
    ) -> Option<usize> {
        let folder_name = teddiebox_download::hex8(directory);
        let stem = teddiebox_download::hex8(file);
        let folder_name = core::str::from_utf8(&folder_name).ok()?;

        let mut file_name = [0u8; 12];
        file_name[..8].copy_from_slice(&stem);
        file_name[8..].copy_from_slice(b".POS");
        let file_name = core::str::from_utf8(&file_name).ok()?;

        let root = if stock { TONIE_CONTENT_DIR } else { CACHE_DIR };
        let top = self.volumes.open_dir(self.root, root).ok()?;
        let folder = match self.volumes.open_dir(top, folder_name) {
            Ok(folder) => folder,
            Err(_) => {
                let _ = self.volumes.close_dir(top);
                return None;
            }
        };
        let _ = self.volumes.close_dir(top);

        let handle = self
            .volumes
            .open_file_in_dir(folder, file_name, Mode::ReadOnly);
        let _ = self.volumes.close_dir(folder);
        let handle = handle.ok()?;

        let filled = self.volumes.read(handle, buffer).ok();
        self.close_file(handle);
        filled
    }

    /// Writes `/CONTENT|CACHE/<directory>/<file>.POS`.
    ///
    /// Truncates the file first: it holds one number, and leftover digits
    /// from a longer old number would change it.
    ///
    /// Under `CACHE` the directory may be created here. Under `CONTENT` it is
    /// not: a missing stock directory means the story is not there.
    pub fn write_position(
        &self,
        stock: bool,
        directory: u32,
        file: u32,
        bytes: &[u8],
    ) -> Result<(), &'static str> {
        let folder_name = teddiebox_download::hex8(directory);
        let stem = teddiebox_download::hex8(file);
        let folder_name =
            core::str::from_utf8(&folder_name).map_err(|_| "the directory name is not text")?;

        let mut file_name = [0u8; 12];
        file_name[..8].copy_from_slice(&stem);
        file_name[8..].copy_from_slice(b".POS");
        let file_name =
            core::str::from_utf8(&file_name).map_err(|_| "the position name is not text")?;

        let top = if stock {
            self.volumes
                .open_dir(self.root, TONIE_CONTENT_DIR)
                .map_err(|_| "no CONTENT directory")?
        } else {
            self.open_or_make_dir(self.root, CACHE_DIR)?
        };
        let folder = if stock {
            self.volumes
                .open_dir(top, folder_name)
                .map_err(|_| "no story directory")
        } else {
            self.open_or_make_dir(top, folder_name)
        };
        let _ = self.volumes.close_dir(top);
        let folder = folder?;

        let handle =
            self.volumes
                .open_file_in_dir(folder, file_name, Mode::ReadWriteCreateOrTruncate);
        let _ = self.volumes.close_dir(folder);
        let handle = handle.map_err(|_| "the position file would not open")?;

        let wrote = self
            .volumes
            .write(handle, bytes)
            .map_err(|_| "the position write failed")
            .and_then(|()| {
                self.volumes
                    .flush_file(handle)
                    .map_err(|_| "the position flush failed")
            });
        self.close_file(handle);
        wrote
    }

    /// Opens a subdirectory, creating it if it is not there yet.
    ///
    /// Tries to open first, and creates only if that fails.
    fn open_or_make_dir(
        &self,
        parent: RawDirectory,
        name: &str,
    ) -> Result<RawDirectory, &'static str> {
        if let Ok(dir) = self.volumes.open_dir(parent, name) {
            return Ok(dir);
        }
        self.volumes
            .make_dir_in_dir(parent, name)
            .map_err(|_| "could not create a directory on the card")?;
        self.volumes
            .open_dir(parent, name)
            .map_err(|_| "the directory would not open after being created")
    }

    /// Appends to a cache file, **seeking to the end first**.
    ///
    /// The handle is shared with the reader, so the offset may be wherever
    /// the decoder left it; writing there would overwrite part of the story.
    pub fn append(&self, file: RawFile, bytes: &[u8]) -> Result<(), &'static str> {
        self.volumes
            .file_seek_from_end(file, 0)
            .map_err(|_| "could not seek to the end")?;
        self.volumes
            .write(file, bytes)
            .map_err(|_| "the write failed")
    }

    /// Saves what has been written and updates the file's recorded length.
    pub fn flush(&self, file: RawFile) -> Result<(), &'static str> {
        self.volumes
            .flush_file(file)
            .map_err(|_| "the flush failed")
    }

    /// Reads a certificate off the card, by name — in practice, `TCCA.DER`,
    /// the authority the server is verified against.
    ///
    /// Returns how many bytes were read. A read that fills the buffer is
    /// refused rather than cut short, since part of a certificate is useless.
    pub fn read_certificate(
        &self,
        name: &str,
        buffer: &mut [u8],
    ) -> Result<usize, CertificateError> {
        let dir = self
            .volumes
            .open_dir(self.root, CERT_DIR)
            .map_err(|_| CertificateError::Missing)?;
        let file = self.volumes.open_file_in_dir(dir, name, Mode::ReadOnly);
        let _ = self.volumes.close_dir(dir);
        let file = file.map_err(|_| CertificateError::Missing)?;

        let mut filled = 0;
        let outcome = loop {
            if filled == buffer.len() {
                break Err(CertificateError::TooLarge);
            }
            match self.volumes.read(file, &mut buffer[filled..]) {
                Ok(0) => break Ok(filled),
                Ok(n) => filled += n,
                Err(_) => break Err(CertificateError::Unreadable),
            }
        };
        self.close_file(file);
        outcome
    }

    /// Writes a certificate into the card's `CERT` directory, creating the
    /// directory if needed and replacing any file of that name.
    pub fn write_certificate(&self, name: &str, bytes: &[u8]) -> Result<(), &'static str> {
        let dir = self.open_or_make_dir(self.root, CERT_DIR)?;
        let file = self
            .volumes
            .open_file_in_dir(dir, name, Mode::ReadWriteCreateOrTruncate);
        let _ = self.volumes.close_dir(dir);
        let file = file.map_err(|_| "the certificate would not open for writing")?;

        let outcome = self
            .volumes
            .write(file, bytes)
            .map_err(|_| "the certificate would not write")
            .and_then(|()| {
                self.volumes
                    .flush_file(file)
                    .map_err(|_| "the certificate would not flush")
            });
        self.close_file(file);
        outcome
    }

    /// Opens a file in the root directory for reading.
    pub fn open_file(&self, name: ShortFileName) -> Result<RawFile, &'static str> {
        self.volumes
            .open_file_in_dir(self.root, name, Mode::ReadOnly)
            .map_err(|_| "the file would not open")
    }

    /// Reads the next bytes of an open file, returning how many arrived.
    pub fn read(&self, file: RawFile, buffer: &mut [u8]) -> Result<usize, &'static str> {
        self.volumes.read(file, buffer).map_err(|_| "read failed")
    }

    pub fn seek(&self, file: RawFile, offset: u32) -> Result<(), &'static str> {
        self.volumes
            .file_seek_from_start(file, offset)
            .map_err(|_| "seek failed")
    }

    pub fn close_file(&self, file: RawFile) {
        let _ = self.volumes.close_file(file);
    }
}

/// Checksums every file under `root`, depth first.
///
/// A loop, not recursion: [`Cursor`] holds the position, so this can `await`
/// between entries and during long reads. Open directory handles are kept in
/// an array indexed by depth, which is why [`Volumes`] allows `MAX_DEPTH`
/// directories.
async fn walk_tree(volumes: &Volumes, root: RawDirectory) {
    let mut path = Path::new();
    let mut cursor = Cursor::new();
    let mut dirs: [Option<RawDirectory>; MAX_DEPTH] = [None; MAX_DEPTH];
    // Where to shorten the path to when leaving each level.
    let mut marks = [0usize; MAX_DEPTH];
    dirs[0] = Some(root);

    loop {
        let Some(here) = dirs[cursor.depth()] else {
            // Cannot happen, given how the cursor moves; stop rather than
            // walk a directory without a handle.
            esp_println::println!("teddiebox: sd lost its place");
            return;
        };

        let entry = nth_entry(volumes, here, cursor.index() as usize);
        let found = match &entry {
            None => Found::Nothing,
            Some(entry) if entry.is_dir => Found::Directory,
            Some(_) => Found::File,
        };

        match cursor.advance(found) {
            Action::ReadFile => {
                let entry = entry.expect("a file was found at this position");
                checksum_file(volumes, here, &mut path, &entry).await;
            }

            Action::Descend => {
                let entry = entry.expect("a directory was found at this position");
                let depth = cursor.depth();
                marks[depth] = path.push(&entry.name);

                match volumes.open_dir(here, entry.name) {
                    Ok(child) => dirs[depth] = Some(child),
                    Err(_) => {
                        esp_println::println!("teddiebox: sd {} UNREADABLE", path.as_str());
                        // Treat it as an empty directory: the cursor goes back
                        // to the parent and steps past it.
                        if cursor.advance(Found::Nothing) == Action::Ascend {
                            path.truncate(marks[depth]);
                        }
                    }
                }
            }

            Action::TooDeep => {
                let entry = entry.expect("a directory was found at this position");
                let mark = path.push(&entry.name);
                esp_println::println!("teddiebox: sd {} SKIPPED too deep", path.as_str());
                path.truncate(mark);
            }

            Action::Ascend => {
                // The cursor has already moved up, so the level just left is
                // one below it.
                let left = cursor.depth() + 1;
                if let Some(directory) = dirs[left].take() {
                    let _ = volumes.close_dir(directory);
                }
                path.truncate(marks[left]);
            }

            Action::Finished => return,
        }

        // Yield at every entry, not only in long reads: many small files
        // would otherwise block the executor too.
        yield_now().await;
    }
}

/// The `index`-th real entry of `dir`, ignoring `.`, `..` and the volume label.
///
/// Separate from the walk because `iterate_dir` holds the volume manager
/// while its closure runs, so nothing can be opened inside it.
fn nth_entry(volumes: &Volumes, dir: RawDirectory, index: usize) -> Option<Entry> {
    let mut seen = 0;
    let mut found = None;

    let result = volumes.iterate_dir(dir, |entry| {
        if entry.attributes.is_volume()
            || entry.name == ShortFileName::this_dir()
            || entry.name == ShortFileName::parent_dir()
        {
            return ControlFlow::Continue(());
        }

        if seen == index {
            found = Some(Entry {
                name: entry.name,
                size: entry.size,
                is_dir: entry.attributes.is_directory(),
            });
            return ControlFlow::Break(());
        }

        seen += 1;
        ControlFlow::Continue(())
    });

    if result.is_err() {
        esp_println::println!("teddiebox: sd directory unreadable");
        return None;
    }
    found
}

/// Reads one file end to end and prints its checksum.
///
/// The line is `path size crc32`, as the host script prints. The throughput
/// after it is not compared; it shows how fast this bus reads.
async fn checksum_file(volumes: &Volumes, dir: RawDirectory, path: &mut Path, entry: &Entry) {
    let mark = path.push(&entry.name);

    let file = match volumes.open_file_in_dir(dir, entry.name, Mode::ReadOnly) {
        Ok(file) => file,
        Err(_) => {
            esp_println::println!("teddiebox: sd {} UNREADABLE", path.as_str());
            path.truncate(mark);
            return;
        }
    };

    let mut crc = Crc32::new();
    let mut buffer = [0u8; 512];
    let mut read: u64 = 0;
    let started = Instant::now();
    let mut failed = false;
    let mut blocks: u32 = 0;

    loop {
        match volumes.read(file, &mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                crc.update(&buffer[..n]);
                read += n as u64;
            }
            Err(_) => {
                failed = true;
                break;
            }
        }

        // A large file can take minutes to read, so yield inside this loop
        // too.
        blocks += 1;
        if blocks.is_multiple_of(YIELD_EVERY_BLOCKS) {
            yield_now().await;
        }
    }

    let _ = volumes.close_file(file);

    if failed {
        esp_println::println!(
            "teddiebox: sd {} READ FAILED after {read} of {} bytes",
            path.as_str(),
            entry.size
        );
        path.truncate(mark);
        return;
    }

    // Report a short read, rather than a checksum of part of the file.
    if read != u64::from(entry.size) {
        esp_println::println!(
            "teddiebox: sd {} SHORT read {read} of {} bytes",
            path.as_str(),
            entry.size
        );
        path.truncate(mark);
        return;
    }

    let elapsed_ms = started.elapsed().as_millis().max(1);
    esp_println::println!(
        "teddiebox: sd {} {} {:08X} ({} KiB/s)",
        path.as_str(),
        entry.size,
        crc.finish(),
        read * 1000 / elapsed_ms / 1024
    );

    path.truncate(mark);
}

/// The bus configuration the card must be initialised at.
pub fn init_config() -> SpiConfig {
    SpiConfig::default().with_frequency(Rate::from_khz(INIT_RATE_KHZ))
}
