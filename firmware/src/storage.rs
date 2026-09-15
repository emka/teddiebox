//! The SD card, in SPI mode.
//!
//! Bench step 7: the card mounts as FAT and every file on it checksums equal
//! to the same file read on a laptop. `scripts/sd-checksums.sh` is the other
//! half of that comparison and prints the same three columns, so the criterion
//! is a `diff` rather than an eyeball.
//!
//! Read-only, deliberately. The card in the box is the one the box shipped
//! with, and it is evidence: nothing here opens a file for writing, creates
//! one, or touches a directory.
//!
//! This module owns pins and a bus. The checksum it reports is
//! [`teddiebox_core::checksum`] and the traversal order comes from
//! [`teddiebox_core::walk`]; neither knows anything about cards, which is what
//! lets both be tested on the host.
//!
//! **The walk yields.** It began as a recursive blocking function, and on a
//! full card that held the single executor for 78 minutes: the heartbeat froze
//! at one tick, the ears went unpolled and the console stopped reading, so the
//! box could not be told to stop and only pulling power ended it. Worse, a
//! file prints only once it is fully read, so a 114 MB file was seven minutes
//! of silence indistinguishable from a hang. The traversal is now a cursor
//! rather than a call stack, which is what lets this be a flat `async` loop
//! that can await — recursion would need its futures boxed, and this firmware
//! deliberately does not allocate outside the radio stack, whose heap is the
//! one allocator on the box and is sized for the Wi-Fi driver alone.
//!
//! **Names are the 8.3 short names**, because that is what the directory
//! entries hold. A file stored with a long name prints here as its short alias
//! and on the host as its long one, so the two would disagree about the path
//! while agreeing about every byte. A Toniebox card is `CONTENT/<8 hex>/<8
//! hex>` throughout and has no long names, but a card that did would show it
//! as a path difference in the diff rather than a checksum difference.

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
/// Conservative on purpose. Step 7 asks whether the bytes are right, not how
/// fast they arrive, and a checksum mismatch caused by clocking a card too
/// hard would be read as a filesystem fault. The throughput this prints is
/// what a later step would use to argue for more.
const WALK_RATE_KHZ: u32 = 8_000;

/// How often the file read hands the executor back.
///
/// Every 16 blocks is 8 KiB, about 30 ms at the rate this bus sustains — often
/// enough that the heartbeat keeps time and the console stays responsive,
/// rarely enough that yielding is not what the walk spends its time on.
const YIELD_EVERY_BLOCKS: u32 = 16;

/// Microseconds spent inside [`CardPages::read_page`].
///
/// Playback measures how much of real time it costs to keep the codec fed, but
/// that figure covers the decode *and* the card reads that feed it, because
/// the reads happen underneath `next_frame`. Which of the two dominates
/// decides which lever is worth pulling — a faster SPI clock, or a cheaper
/// decoder — so they are counted apart.
///
/// 64-bit through `portable-atomic`, because Xtensa has no native 64-bit
/// atomic and microseconds in a `u32` wrap after about seventy minutes — well
/// inside the length of a single Tonie, and a wrapped figure would read as a
/// suspiciously fast decode rather than as an error.
pub static PAGE_READ_US: AtomicU64 = AtomicU64::new(0);

/// The longest single page read, in microseconds.
///
/// The total says how much of real time the card costs; this says whether any
/// *one* read was long enough to matter. Those are different questions and only
/// the second explains a missed deadline: the audio cushion is about 170 ms, so
/// a single read over that starves the DMA however small the average is.
pub static PAGE_READ_MAX_US: AtomicU64 = AtomicU64::new(0);

/// Where a Toniebox keeps its audio, and what it calls it.
///
/// Fixed by the box's own layout rather than chosen here: every file step 7
/// found on the real card sat at `CONTENT/<8 hex>/500304E0`.
const TONIE_CONTENT_DIR: &str = "CONTENT";

/// Where downloads are cached, laid out exactly like `CONTENT`.
///
/// Same `<8 hex>/<8 hex>` split, so a file that finishes downloading sits
/// where a stock one would and nothing has to translate between two
/// conventions later.
const CACHE_DIR: &str = "CACHE";

/// Where the box's own certificates live, mirroring the `cert/` directory the
/// stock firmware keeps them in on flash.
///
/// The same three files a teddyCloud setup extracts: `CLIENT.DER` identifies
/// this box to the server, `PRIVATE.DER` proves it, `CA.DER` is Boxine's own
/// authority. All three are 8.3 names already, so `open_file_in_dir` reaches
/// them directly.
const CERT_DIR: &str = "CERT";

/// Why the card's configuration could not be used.
///
/// Three cases, kept apart because they call for different words: no file at
/// all is the ordinary state of a fresh card, a file that will not read is a
/// card or wiring fault, and a file that will not parse is a typo somebody can
/// fix.
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

/// Writes `value` as eight upper-case hex digits, the way FAT holds a
/// Toniebox content name.
fn write_hex8(out: &mut [u8; 8], value: u32) {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = DIGITS[((value >> (28 - 4 * i)) & 0xF) as usize];
    }
}

/// Longest path the walk will print.
const MAX_PATH: usize = 64;

/// FAT stamps a timestamp on anything it creates. Nothing here creates
/// anything, so this is never read back — but the type demands one.
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
/// One of these exists for the whole walk and is pushed and popped as it
/// descends, so the path costs the same whatever the depth. Building it per
/// level instead would put a buffer on every stack frame.
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
    /// Silently stops at the buffer's end rather than wrapping: a truncated
    /// path prints wrongly, which the host diff catches, where a wrapped one
    /// would print plausibly and be believed.
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
        // Every byte written came from a ShortFileName, which is 8.3 ASCII.
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
/// `teddiebox-taf` asks for 4096-byte pages and nothing else, because every
/// structure in a TAF file is 4096-aligned. On the host that trait is a slice
/// index; here it is a seek and a read, and nothing above it knows which.
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
            // A short final page is normal — the last Ogg page runs only as
            // long as it needs to — so this rounds up rather than truncating.
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

        // One read is not guaranteed to fill the buffer, so keep asking until
        // it does or the file ends.
        let mut filled = 0;
        while filled < PAGE_SIZE {
            let read = self.card.read(self.file, &mut buf[filled..])?;
            if read == 0 {
                break;
            }
            filled += read;
        }

        // Zero-fill a short final page. Safe because an Ogg page declares its
        // own extent through its lacing table, so nothing reads past the bytes
        // that were really there.
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

/// The volume manager, sized to what the walk actually holds open at once.
///
/// The directory limit is [`MAX_DEPTH`] and not a number of its own: the walk
/// keeps every directory on the current path open, so it needs exactly as many
/// as it is deep. Setting them independently would let the depth limit grow
/// past the handle limit, and the walk would then stop with `TooManyOpenDirs`
/// on a card that is merely deep — which reads as an unreadable directory.
/// **Two files**, and there is one volume. One was enough while the card was
/// only ever read, and it is what a download makes insufficient: the cache file
/// stays open for the length of a download, and with a single handle the box
/// could not open *anything else* while one ran — no story, no system sound.
/// A box that cannot play while it downloads is the thing this design exists to
/// avoid, so the second handle is not a convenience.
///
/// Two, not more: reading and writing the *same* file still shares one handle,
/// because `embedded-sdmmc` refuses a second on one file whatever this says.
type Volumes = VolumeManager<Card, NoClock, MAX_DEPTH, 2, 1>;

/// A mounted card, and the handles that keep it open.
///
/// Nothing closes it. Once the card is up it stays up for as long as the box
/// runs, because every command that wants it wants it again, and a rail shared
/// with the NFC reader is not one to cycle for the sake of tidiness.
///
/// Mounting is separate from walking because step 8 wants the same card for a
/// different purpose. Doing it once and keeping the handles also avoids
/// re-identifying the card and re-raising the bus clock every time a bench
/// command is typed.
pub struct Mounted {
    volumes: Volumes,
    root: RawDirectory,
}

impl Mounted {
    /// Brings the card up and mounts partition 0.
    ///
    /// The storage rail must already be on; powering it is the caller's
    /// business, because the rail is shared with the NFC reader and this
    /// module has no claim on that decision.
    pub fn open(
        spi: Spi<'static, esp_hal::Blocking>,
        cs: Output<'static>,
        delay: Delay,
    ) -> Result<Self, &'static str> {
        let device =
            ExclusiveDevice::new(spi, cs, delay).map_err(|_| "chip select would not drive")?;
        let card = SdCard::new(device, delay);

        // The first transaction is what actually identifies the card, so this
        // is where a dead bus shows up rather than at construction.
        let bytes = card.num_bytes().map_err(|_| "no card answered")?;
        esp_println::println!(
            "teddiebox: card {} MiB, type {:?}",
            bytes / (1024 * 1024),
            card.get_card_type()
        );

        // Identified, so the bus can leave the specification's initialisation
        // speed behind.
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

    /// Checksums everything on the card. Bench step 7.
    pub async fn walk(&self) {
        esp_println::println!("teddiebox: sd walk begins");
        walk_tree(&self.volumes, self.root).await;
        esp_println::println!("teddiebox: sd walk done");
    }

    /// The first file in the root directory with this extension.
    ///
    /// Root only, and deliberately: the files steps 8 and 9 play are ones
    /// somebody copies onto the card, the root is where they naturally land,
    /// and searching the whole card would mean reading every directory first.
    /// A real Tonie's `.taf` lives three levels down under `CONTENT`, so for
    /// step 9 it has to be copied out to the root — which also keeps step 7's
    /// walk comparing like with like.
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
    /// A real card keeps its content at `CONTENT/<8 hex>/500304E0` — three
    /// levels down, and with no extension, so neither the root search above
    /// nor an extension match will find it. The name is fixed by the
    /// Toniebox's own layout, which is the layout this box exists to read.
    ///
    /// Read-only, and that is the point: it means step 9 can run against the
    /// card the box shipped with, without copying anything onto it.
    ///
    /// The directory handles are closed before returning. An open file keeps
    /// the directory entry it was opened from, so it does not need its parent
    /// to stay open — and holding three levels open would sit right on the
    /// `MAX_DIRS` limit.
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

    /// Opens `CACHE/<directory>/<file>`, where a download lands.
    ///
    /// The same layout as `CONTENT`, deliberately, so a file that finishes
    /// downloading sits where a stock one would and this is the same call with
    /// a different tree.
    pub fn open_cache(&self, directory: u32, file: u32) -> Result<(RawFile, u32), &'static str> {
        self.open_audio_under(CACHE_DIR, directory, file)
    }

    /// Opens `<tree>/<directory>/<file>` for reading, and says how long it is.
    ///
    /// Names are the eight upper-case hex digits FAT stores, formatted here
    /// rather than taken from the caller: a lower-case or short name is a
    /// different file on a FAT volume, and the failure would be "not found"
    /// rather than anything that points at the cause.
    fn open_audio_under(
        &self,
        tree: &str,
        directory: u32,
        file: u32,
    ) -> Result<(RawFile, u32), &'static str> {
        let mut folder_name = [0u8; 8];
        let mut file_name = [0u8; 8];
        write_hex8(&mut folder_name, directory);
        write_hex8(&mut file_name, file);
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

        // The size comes from the directory entry rather than from the open
        // file, because the decoder needs to know where the last page ends
        // before it reads any of them.
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
    /// The card is the media task's, and the radio must never open a file —
    /// so this is the one place `CONFIG.TXT` is read, and what comes back is a
    /// parsed value rather than a handle.
    ///
    /// `buffer` comes from the caller because this is called once at boot and
    /// a permanent buffer for a one-shot read would be paid for forever. Its
    /// size is the file-size limit: a read that fills it is refused rather
    /// than parsed, since a config cut mid-line still parses and does so into
    /// a value nobody typed.
    pub fn read_config(&self, buffer: &mut [u8]) -> Result<Config, ConfigTrouble> {
        let name = ShortFileName::create_from_str(teddiebox_config::FILENAME)
            .map_err(|_| ConfigTrouble::Missing)?;
        let file = self.open_file(name).map_err(|_| ConfigTrouble::Missing)?;

        let mut filled = 0;
        let outcome = loop {
            if filled == buffer.len() {
                // Full, with no way to know whether more was coming.
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
    /// Separate from [`Mounted::read_config`], which answers a parsed
    /// `Config`: the portal shows somebody their file to edit, comments and
    /// all, so it needs the bytes and not the meaning.
    ///
    /// A missing file answers `Ok(0)`. That is a box being set up for the
    /// first time, which is the case this whole path exists for — not an
    /// error to report.
    pub fn read_config_bytes(&self, buffer: &mut [u8]) -> Result<usize, &'static str> {
        let name = ShortFileName::create_from_str(teddiebox_config::FILENAME)
            .map_err(|_| "the config name is not a short name")?;
        let Ok(file) = self.open_file(name) else {
            return Ok(0);
        };

        let mut filled = 0;
        let outcome = loop {
            if filled == buffer.len() {
                // Full is not the same as too long, and treating it as such
                // made a file of exactly `buffer.len()` bytes unreadable —
                // which is a length this box will happily *write*, so saving
                // one left the page empty and erroring for ever. Ask for one
                // more byte instead: if there is not one, the file ends
                // exactly here and it fits.
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
    /// **Not atomic, and it cannot be**: `embedded-sdmmc` 0.10 has no rename,
    /// so there is no way to write beside the file and swap. A power cut
    /// between the truncate and the flush leaves a short file and a box that
    /// will not associate.
    ///
    /// That is survivable only because of what is *not* here: the setup
    /// access point's credentials are compiled in and never read from the
    /// card, so a truncated config is exactly the state holding both ears at
    /// boot recovers from. `tools/fat-assumptions` proves the truncation
    /// leaves no tail.
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

    /// Opens `/CACHE/<directory>/<file>` for writing, creating whatever is
    /// missing, and **discards anything already there**.
    ///
    /// Truncating is the honest behaviour until a sidecar exists. Resuming
    /// needs to know how far a previous run got, and the only trustworthy
    /// answer is what survived the last *flush* — `embedded-sdmmc` makes a
    /// file's recorded length truthful only then. Appending without that
    /// record would grow a file past its real content and fail a checksum a
    /// long way from the cause; starting again is slower and correct.
    ///
    /// **One handle, opened once.** The reader and the writer share it — the
    /// library refuses a second handle on the same file — so this is called
    /// once per download and the offset moves between reads and appends, which
    /// is why [`Mounted::append`] seeks to the end itself.
    pub fn open_cache_for_write(&self, directory: u32, file: u32) -> Result<RawFile, &'static str> {
        self.open_cache_in_mode(directory, file, Mode::ReadWriteCreateOrTruncate)
    }

    /// Opens the same file **keeping what is already in it**, to continue an
    /// interrupted download.
    ///
    /// Only ever right when a sidecar vouches for where the file stops and the
    /// server has agreed to carry on from there — [`teddiebox_download::place`]
    /// is what decides that. Appending to a file the server did not resume
    /// splices the start of a story onto its middle, and the result is the
    /// right length, so nothing downstream would catch it.
    pub fn open_cache_for_append(
        &self,
        directory: u32,
        file: u32,
    ) -> Result<RawFile, &'static str> {
        self.open_cache_in_mode(directory, file, Mode::ReadWriteCreateOrAppend)
    }

    fn open_cache_in_mode(
        &self,
        directory: u32,
        file: u32,
        mode: Mode,
    ) -> Result<RawFile, &'static str> {
        let mut folder_name = [0u8; 8];
        let mut file_name = [0u8; 8];
        write_hex8(&mut folder_name, directory);
        write_hex8(&mut file_name, file);
        let folder_name =
            core::str::from_utf8(&folder_name).map_err(|_| "the directory name is not text")?;
        let file_name =
            core::str::from_utf8(&file_name).map_err(|_| "the file name is not text")?;

        // Every handle is closed on the way back out, including on the error
        // paths. Directory handles are a budget of MAX_DEPTH shared with the
        // walk, and leaking two per download meant the *second* download could
        // not create its directory — which reported as an unwritable card
        // rather than as a handle that was never given back.
        let cache = self.open_or_make_dir(self.root, CACHE_DIR)?;
        let folder = self.open_or_make_dir(cache, folder_name);
        let _ = self.volumes.close_dir(cache);
        let folder = folder?;

        let handle = self.volumes.open_file_in_dir(folder, file_name, mode);
        let _ = self.volumes.close_dir(folder);
        handle.map_err(|_| "the cache file would not open")
    }

    /// How long the cached content file is, or `None` if there is not one.
    ///
    /// Opened and closed here rather than handed back: the writer needs one
    /// handle on that file and the library will not give out a second, so the
    /// length has to be learned before the download opens it.
    pub fn cache_length(&self, directory: u32, file: u32) -> Option<u32> {
        let (handle, length) = self.open_cache(directory, file).ok()?;
        self.close_file(handle);
        Some(length)
    }

    /// Reads `/CACHE/<directory>/<file>.MET` into `buffer`.
    ///
    /// Returns how many bytes it held. Every failure is the same answer to the
    /// caller — no sidecar — because a sidecar that cannot be read vouches for
    /// nothing, and the content beside it is incomplete by definition.
    pub fn read_sidecar(&self, directory: u32, file: u32, buffer: &mut [u8]) -> Option<usize> {
        let mut folder_name = [0u8; 8];
        let mut stem = [0u8; 8];
        write_hex8(&mut folder_name, directory);
        write_hex8(&mut stem, file);
        let folder_name = core::str::from_utf8(&folder_name).ok()?;

        let mut file_name = [0u8; 12];
        file_name[..8].copy_from_slice(&stem);
        file_name[8..].copy_from_slice(b".MET");
        let file_name = core::str::from_utf8(&file_name).ok()?;

        // Nothing is created on this path. A read that makes a directory would
        // leave the card littered by the act of asking whether anything is
        // there.
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
        let handle = handle.ok()?;

        let filled = self.volumes.read(handle, buffer).ok();
        self.close_file(handle);
        filled
    }

    /// Writes `/CACHE/<directory>/<file>.MET`, the record that says how long
    /// the content beside it is meant to be.
    ///
    /// **Written before the first body byte, and closed before returning.** It
    /// is only worth having if it survives the interruption that makes it
    /// worth having, and a handle held open for the length of a download is a
    /// handle whose bytes are still in a buffer when the battery goes flat. A
    /// content file with no sidecar beside it is incomplete by definition, so
    /// failing to write this is safe in the direction that matters: the
    /// download is refetched rather than trusted.
    pub fn write_sidecar(
        &self,
        directory: u32,
        file: u32,
        bytes: &[u8],
    ) -> Result<(), &'static str> {
        let mut folder_name = [0u8; 8];
        let mut stem = [0u8; 8];
        write_hex8(&mut folder_name, directory);
        write_hex8(&mut stem, file);
        let folder_name =
            core::str::from_utf8(&folder_name).map_err(|_| "the directory name is not text")?;

        // `<8 hex>.MET`, which is an 8.3 name exactly as FAT wants it.
        let mut file_name = [0u8; 12];
        file_name[..8].copy_from_slice(&stem);
        file_name[8..].copy_from_slice(b".MET");
        let file_name =
            core::str::from_utf8(&file_name).map_err(|_| "the sidecar name is not text")?;

        // Same handle discipline as `open_cache_for_write`: directory handles
        // are a budget shared with the walk, and every path here gives both
        // back before returning.
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

    /// Reads `/CONTENT|CACHE/<directory>/<file>.POS`, the chapter a story
    /// should resume at.
    ///
    /// Deliberately not the `.MET` sidecar. That records what the server said
    /// a download's length is, and `decide()` reads it to tell a complete file
    /// from a partial one — a torn write while saving a chapter would corrupt
    /// the record download resume depends on. Frequently-written convenience
    /// data and rarely-written correctness data belong in different files.
    ///
    /// Nothing is created on this path, exactly as `read_sidecar` creates
    /// nothing: a read that made a directory would litter the card by the act
    /// of asking whether anything is there.
    pub fn read_position(
        &self,
        stock: bool,
        directory: u32,
        file: u32,
        buffer: &mut [u8],
    ) -> Option<usize> {
        let mut folder_name = [0u8; 8];
        let mut stem = [0u8; 8];
        write_hex8(&mut folder_name, directory);
        write_hex8(&mut stem, file);
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
    /// Truncating rather than appending: this file holds one number, and the
    /// tail of a longer previous number left behind would parse as something
    /// else entirely.
    ///
    /// Under `CACHE` the directory is ours and may legitimately be created
    /// here. Under `CONTENT` it is not: a stock directory that is not there is
    /// a story that is not there, and creating one would litter the card on
    /// behalf of a question.
    pub fn write_position(
        &self,
        stock: bool,
        directory: u32,
        file: u32,
        bytes: &[u8],
    ) -> Result<(), &'static str> {
        let mut folder_name = [0u8; 8];
        let mut stem = [0u8; 8];
        write_hex8(&mut folder_name, directory);
        write_hex8(&mut stem, file);
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

    /// Opens a subdirectory, creating it if this is the first download.
    ///
    /// Creating first and opening second would fail on every run after the
    /// first, so it is the other way round.
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
            .map_err(|_| "could not create the cache directory")?;
        self.volumes
            .open_dir(parent, name)
            .map_err(|_| "the cache directory would not open after being created")
    }

    /// Appends to a cache file, **seeking to the end first**.
    ///
    /// The seek is the point. This handle is shared with the reader, so by the
    /// time more bytes arrive the offset is wherever the decoder left it — and
    /// a write there overwrites part of the story instead of extending it,
    /// silently, in bytes the decoder has already been promised.
    pub fn append(&self, file: RawFile, bytes: &[u8]) -> Result<(), &'static str> {
        self.volumes
            .file_seek_from_end(file, 0)
            .map_err(|_| "could not seek to the end")?;
        self.volumes
            .write(file, bytes)
            .map_err(|_| "the write failed")
    }

    /// Makes what has been written durable and the recorded length truthful.
    pub fn flush(&self, file: RawFile) -> Result<(), &'static str> {
        self.volumes
            .flush_file(file)
            .map_err(|_| "the flush failed")
    }

    /// Reads one of the box's certificates off the card.
    ///
    /// Returns how many bytes were read. A read that fills the buffer is
    /// refused rather than truncated: half a DER structure is not a smaller
    /// certificate, it is a parse failure several layers away from here.
    ///
    /// **`PRIVATE.DER` is the key that identifies this box to the tonies
    /// cloud.** It is read into memory and never printed, and the card it sits
    /// on is readable by anything with a card reader — which is a property of
    /// where the stock firmware keeps it too, but worth knowing.
    pub fn read_certificate(&self, name: &str, buffer: &mut [u8]) -> Result<usize, &'static str> {
        let dir = self
            .volumes
            .open_dir(self.root, CERT_DIR)
            .map_err(|_| "no CERT directory on the card")?;
        let file = self.volumes.open_file_in_dir(dir, name, Mode::ReadOnly);
        let _ = self.volumes.close_dir(dir);
        let file = file.map_err(|_| "no such certificate")?;

        let mut filled = 0;
        let outcome = loop {
            if filled == buffer.len() {
                break Err("the certificate is larger than its buffer");
            }
            match self.volumes.read(file, &mut buffer[filled..]) {
                Ok(0) => break Ok(filled),
                Ok(n) => filled += n,
                Err(_) => break Err("the certificate would not read"),
            }
        };
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
/// Flat rather than recursive: [`Cursor`] holds the position the call stack
/// used to, so this can `await` between entries and inside long reads. The
/// open directory handles live in an array indexed by depth, which is why
/// `MAX_DIRS` on [`Volumes`] is `MAX_DEPTH` and not a number of its own.
async fn walk_tree(volumes: &Volumes, root: RawDirectory) {
    let mut path = Path::new();
    let mut cursor = Cursor::new();
    let mut dirs: [Option<RawDirectory>; MAX_DEPTH] = [None; MAX_DEPTH];
    // Where to cut the path back to when each level is left.
    let mut marks = [0usize; MAX_DEPTH];
    dirs[0] = Some(root);

    loop {
        let Some(here) = dirs[cursor.depth()] else {
            // Only reachable if a level were left open with no handle, which
            // the cursor's own transitions rule out. Stopping beats walking a
            // directory this cannot name.
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
                        // Treat it as a directory that turned out to be empty:
                        // the cursor pops back to the parent and steps past it,
                        // which is exactly the recovery wanted and needs no
                        // separate way to undo a descent.
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
                // The cursor has already stepped back, so the level just left
                // is the one below where it now is.
                let left = cursor.depth() + 1;
                if let Some(directory) = dirs[left].take() {
                    let _ = volumes.close_dir(directory);
                }
                path.truncate(marks[left]);
            }

            Action::Finished => return,
        }

        // Hand the executor back at every entry, not only inside long reads:
        // a directory of small files would otherwise starve it just as
        // effectively as one large one.
        yield_now().await;
    }
}

/// The `index`-th real entry of `dir`, ignoring `.`, `..` and the volume label.
///
/// Separate from the walk because `iterate_dir` holds the volume manager for
/// as long as the closure runs: opening a file or a directory from inside it
/// fails. Everything the walk does needs the manager back first.
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
/// The line is `path size crc32`, which is what the host script prints too.
/// The throughput after it is not part of the comparison — it is the first
/// measurement of what this bus actually sustains, which is what a later step
/// would need before arguing for the SDMMC controller instead.
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

        // The largest file on the card is seven minutes of reading. Without a
        // yield inside this loop the box would still go silent for all of it,
        // however often the walk yielded between files.
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

    // A short read is a silent corruption otherwise: the checksum would be of
    // less than the file and would simply disagree with the host, without
    // saying why.
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
