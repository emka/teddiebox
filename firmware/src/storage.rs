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
//! that can await — recursion would need its futures boxed, and there is no
//! allocator here to box them in.
//!
//! **Names are the 8.3 short names**, because that is what the directory
//! entries hold. A file stored with a long name prints here as its short alias
//! and on the host as its long one, so the two would disagree about the path
//! while agreeing about every byte. A Toniebox card is `CONTENT/<8 hex>/<8
//! hex>` throughout and has no long names, but a card that did would show it
//! as a path difference in the diff rather than a checksum difference.

use core::fmt::Write as _;
use core::ops::ControlFlow;

use embassy_futures::yield_now;
use embassy_time::Instant;
use embedded_hal_bus::spi::ExclusiveDevice;
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
/// One file is open at a time, and there is one volume.
type Volumes = VolumeManager<Card, NoClock, MAX_DEPTH, 1, 1>;

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

    /// The first `.WAV` in the root directory.
    ///
    /// Root only, and deliberately: the file is one somebody copies onto the
    /// card for step 8, the root is where it naturally lands, and searching
    /// the whole card would mean reading every directory on it first.
    pub fn find_wav(&self) -> Option<(ShortFileName, u32)> {
        let mut index = 0;
        while let Some(entry) = nth_entry(&self.volumes, self.root, index) {
            index += 1;
            if !entry.is_dir && entry.name.extension() == b"WAV" {
                return Some((entry.name, entry.size));
            }
        }
        None
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
