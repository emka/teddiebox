#![no_std]
#![no_main]

mod audio;
mod battery;
mod flash;
mod i2c_bus;
mod identity;
mod index;
mod inputs;
mod led;
mod libc_shim;
mod net;
mod nfc;
mod ota;
mod pins;
mod portal;
mod sleep;
mod stack;
mod storage;
mod tls;
mod wifikey;

use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};

use critical_section::Mutex as CsMutex;
use embassy_executor::Spawner;
use embassy_futures::select::{select, Either};
use embassy_time::{Duration, Instant, Timer};
use heapless::String;
use teddiebox_config::{Config, Settings};
use teddiebox_console::{MAX_PASSPHRASE, MAX_SSID};

use esp_backtrace as _;
use esp_hal::analog::adc::{Adc, AdcConfig, Attenuation};
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::i2s::master::{Channels, DataFormat, I2s, TdmConfig};
use esp_hal::peripherals::LPWR;
use esp_hal::rtc_cntl::{reset_reason, SocResetReason};
use esp_hal::spi::master::Spi;
use esp_hal::system::Cpu;
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_hal::uart::{Config as UartConfig, UartRx};
use teddiebox_board::{self as board, Gates, Rail};
use teddiebox_console::{Command, CommandWatch};
use teddiebox_core::cue::Cue;
use teddiebox_core::pipe::Pipe;
use teddiebox_core::place::PendingPlace;
use teddiebox_core::plate::{Answering, Placed};
use teddiebox_core::position::{self, MAX_POSITION};
use teddiebox_core::sounds::{Language, Sound};
use teddiebox_core::tone;

use teddiebox_core::{
    colour_for, Action, Core, CoreConfig, Ear, Event, Freshness, LedState, Output as AudioOutput,
    Position, PowerOffReason, TagUid, Unavailable,
};
use teddiebox_download::{
    Bytes, CardSays, ContentSink, Handshake, Landing, Pages, Placement, Step, Throttle, Writer,
};

use crate::index::CardIndex;
use crate::pins::BoardPins;

/// Bytes of heap for the Wi-Fi stack, which mbedtls shares (see `tls.rs`).
///
/// Estimated, not measured. `ControllerConfig::default()` asks the driver for
/// 10 static RX buffers of about 1.6 KB each, plus 32 dynamic RX and 32
/// dynamic TX buffers; mbedtls adds its record buffers and cipher contexts.
const RADIO_HEAP: usize = 88 * 1024;

// The ESP-IDF bootloader identifies an app by this descriptor. Without it the
// image still links, but flashing tools reject it.
//
// The version comes from build.rs (`git describe`), not CARGO_PKG_VERSION,
// which never changes and so could not trigger an update.
esp_bootloader_esp_idf::esp_app_desc!(
    env!("TEDDIEBOX_VERSION"),
    env!("CARGO_PKG_NAME"),
    esp_bootloader_esp_idf::BUILD_TIME,
    esp_bootloader_esp_idf::BUILD_DATE,
    esp_bootloader_esp_idf::ESP_IDF_COMPATIBLE_VERSION,
    esp_bootloader_esp_idf::MMU_PAGE_SIZE,
    0,
    u16::MAX,
    esp_bootloader_esp_idf::SECURE_VERSION
);

/// Reboots into the application.
///
/// The power rails go down first, as for download mode. Lets a laptop
/// restart the box without `esptool`, which needs exclusive use of the serial
/// port.
fn reboot(board: &mut BoardPins, gates: &mut Gates) -> ! {
    esp_println::println!("teddiebox: rebooting");
    drain_console();
    board.apply_all(&gates.release_for_reset());
    esp_hal::system::software_reset()
}

/// Lets the last words out before the reset swallows them.
///
/// `software_reset` does not wait for the UART, so the last lines printed
/// would be cut off. Blocking, because the callers never return.
fn drain_console() {
    esp_hal::delay::Delay::new().delay_millis(20);
}

/// Reboots into the ROM's UART download mode.
///
/// The ROM checks a bit in the RTC's OPTION1 register as well as the GPIO0
/// strapping pin, so the firmware can ask for download mode on the next
/// reset, without shorting J100 (see HARDWARE.md) or a power cycle.
///
/// The power rails go down first, so GPIO45 (a strapping pin) is low at
/// reset.
fn reboot_to_download(board: &mut BoardPins, gates: &mut Gates) -> ! {
    esp_println::println!("teddiebox: rebooting into download mode");
    drain_console();
    board.apply_all(&gates.release_for_reset());

    // Re-enable the USB pads before rebooting.
    //
    // GPIO19 is the chip's USB D- line, and esp-hal disables the USB pads when
    // it becomes the red LED output. `USB_DEVICE.conf0()` survives a software
    // reset, and download mode starts USB Serial/JTAG as well as UART0
    // (`DOWNLOAD(USB/UART0)`). With the pads disabled, the ROM panics right
    // after `waiting for download`.
    esp_hal::peripherals::USB_DEVICE::regs()
        .conf0()
        .modify(|_, w| {
            w.usb_pad_enable().set_bit();
            w.dp_pullup().set_bit()
        });

    esp_hal::peripherals::LPWR::regs()
        .option1()
        .modify(|_, w| w.force_download_boot().set_bit());

    esp_hal::system::software_reset()
}

/// What the reducer last decided the LED should say.
///
/// The reducer runs in the media task; the LED belongs to the console loop.
/// One byte passes between them, like `OUTPUT_REQUEST` and `VOLUME_REQUEST`.
/// It holds a state, not a colour; `teddiebox_core::led::colour_for` maps
/// states to colours.
static LED_REQUEST: AtomicU8 = AtomicU8::new(LedState::Booting.code());

/// What the console has asked the media task to do.
///
/// One value rather than a flag each, because all of these need the same
/// hardware (the I2S peripheral and the SD bus), which the media task owns.
/// None of them runs at boot.
static REQUEST: AtomicU8 = AtomicU8::new(REQUEST_NONE);
const REQUEST_NONE: u8 = 0;
const REQUEST_TONE: u8 = 1;
const REQUEST_WALK: u8 = 2;
const REQUEST_WAV: u8 = 3;
const REQUEST_TAF: u8 = 4;
const REQUEST_CONTENT: u8 = 5;
const REQUEST_PCM: u8 = 6;
const REQUEST_CACHE: u8 = 7;
const REQUEST_CRC: u8 = 8;

/// Set when the console asks the radio for a scan.
///
/// Separate from `NFC_REQUEST`, because the radio and the reader share no
/// hardware.
static NET_REQUEST: AtomicU8 = AtomicU8::new(REQUEST_NONE);
const NET_SCAN: u8 = 1;
const NET_UP: u8 = 2;
const NET_DOWN: u8 = 3;
const NET_STATUS: u8 = 4;
const NET_TLS: u8 = 5;
const NET_GET: u8 = 6;
/// Join and open one TLS session with nobody waiting on it, once per boot, so
/// the first figure placed finds a remembered access point, a set-up key and a
/// session to resume. See [`tls::prime`].
const NET_PRIME: u8 = 7;

/// A figure's identifier and the token that authorises fetching its story.
///
/// Kept together, like
/// [`Seen::Figure`](teddiebox_core::plate::Seen::Figure), so a fetch can
/// never be sent for one figure with another figure's token. With two
/// separate statics, a write to one between the other's write and the fetch
/// could mix them.
#[derive(Debug, Clone, Copy)]
struct FetchRequest {
    /// The identifier in the byte order the console `get` command and the
    /// request line use: the reverse of the order the reader reports.
    ruid: u64,
    token: Option<[u8; 32]>,
    /// Whether this is a question rather than a download.
    ///
    /// A probe uses the same request, token and radio handling as a fetch,
    /// and only asks for less. Marking it here, rather than with a second
    /// request type, means a probe and a fetch can never be queued at once.
    probe: bool,
}

/// The fetch about to be raised on [`NET_GET`], written whole in one go.
///
/// Written by the console `get` command and by the plate's `RequestContent`
/// action, each as a whole [`FetchRequest`] just before raising the request.
static FETCH_REQUEST: CsMutex<RefCell<Option<FetchRequest>>> = CsMutex::new(RefCell::new(None));

/// The net task's answer to a probe, and the figure it is about, as one value.
///
/// One value, not several atomics, so the answer and its figure are always
/// read together. The figure is included so an answer that arrives after the
/// figure was lifted or swapped can be ignored.
static PROBE_ANSWER: CsMutex<RefCell<Option<(u64, teddiebox_download::Answer)>>> =
    CsMutex::new(RefCell::new(None));

/// Publishes a probe's answer, replacing any not yet read.
fn post_answer(ruid: u64, answer: teddiebox_download::Answer) {
    critical_section::with(|cs| *PROBE_ANSWER.borrow_ref_mut(cs) = Some((ruid, answer)));
}

/// Takes the waiting answer, if there is one.
fn take_answer() -> Option<(u64, teddiebox_download::Answer)> {
    critical_section::with(|cs| PROBE_ANSWER.borrow_ref_mut(cs).take())
}

/// Whether an answer is waiting to be read.
fn answer_waiting() -> bool {
    critical_section::with(|cs| PROBE_ANSWER.borrow_ref(cs).is_some())
}

/// The cached file the server has contradicted, as `CACHE/<dir>/<file>`, if any.
///
/// Set by a probe that finds the cached copy out of date, and taken by the
/// download that replaces it. One value, so the path is always read and
/// cleared whole. The old story stays playable until that download starts
/// writing, so a failed download does not lose it.
static STALE: CsMutex<RefCell<Option<teddiebox_download::ContentPath>>> =
    CsMutex::new(RefCell::new(None));

/// Whether this cache entry is the one a probe has just contradicted.
///
/// Clears the mark, so a second download of the same file does not start from
/// zero again. A mark for another file is left for that file's download.
fn take_stale(dir: u32, file: u32) -> bool {
    let path = teddiebox_download::ContentPath {
        directory: dir,
        file,
    };
    critical_section::with(|cs| {
        let mut stale = STALE.borrow_ref_mut(cs);
        if *stale != Some(path) {
            return false;
        }
        *stale = None;
        true
    })
}

/// The question the box is waiting on the server for, if any.
///
/// Used only by the media task: `perform` starts the question and the loop
/// settles it. A static rather than a local, because `perform` is reached
/// through `apply` from many places.
static REVALIDATION: CsMutex<RefCell<teddiebox_download::Revalidation>> =
    CsMutex::new(RefCell::new(teddiebox_download::Revalidation::new()));

/// Which figures have been asked about since the box booted.
///
/// Kept in RAM and cleared on every reset, which sets how often figures are
/// checked: the box turns itself off after five idle minutes, so one boot is
/// about one session.
static ASKED: CsMutex<RefCell<teddiebox_download::Asked>> =
    CsMutex::new(RefCell::new(teddiebox_download::Asked::new()));

/// Whether this figure has already been asked about this session.
pub(crate) fn already_asked(tag: TagUid) -> bool {
    critical_section::with(|cs| ASKED.borrow_ref(cs).contains(tag.ruid()))
}

/// Bytes waiting to move from the network to the card.
///
/// The two ends run in different tasks and neither may wait for the other:
/// the media task would stop playing whenever the server paused, and the
/// network task would stall the socket during a decode.
///
/// Sized from the measured download rate: at ~47 KB/s about 4.7 KB arrive
/// between the media task's reads, so 8 KiB leaves room when a read is late.
/// A full pipe just slows the download; it is not an error.
const DOWNLOAD_PIPE_BYTES: usize = 8192;
static DOWNLOAD_PIPE: CsMutex<RefCell<Pipe<DOWNLOAD_PIPE_BYTES>>> =
    CsMutex::new(RefCell::new(Pipe::new()));

/// Nothing is downloading.
const DOWNLOAD_IDLE: u8 = 0;
/// The producer is still reading from the network.
const DOWNLOAD_RUNNING: u8 = 1;
/// The producer has stopped. Whatever is still in the pipe is the last of it.
const DOWNLOAD_ENDED: u8 = 2;
/// The card is being asked what it already has. Only its owner can answer, and
/// the answer decides what the request asks for, so the producer waits here.
const DOWNLOAD_PREPARING: u8 = 3;
/// The card has answered. The producer may build its request.
const DOWNLOAD_PLANNED: u8 = 4;
static DOWNLOAD_STATE: AtomicU8 = AtomicU8::new(DOWNLOAD_IDLE);
/// Bytes the producer has handed to the pipe. The consumer is done when it has
/// written this many and the producer has ended.
static DOWNLOAD_SENT: AtomicU32 = AtomicU32::new(0);
static DOWNLOAD_DIR: AtomicU32 = AtomicU32::new(0);
static DOWNLOAD_FILE: AtomicU32 = AtomicU32::new(0);

/// How often the producer looks for an answer while a handshake is in
/// progress.
///
/// Shorter than [`teddiebox_download::RETRY_MS`], because this is the delay
/// between the card answering and the producer noticing. When to ask again,
/// and when to give up, are decided by the download crate.
///
/// **A minimum, not a period.** While a story plays, the media task blocks the
/// executor for long stretches, so this timer fires only about every 106 ms
/// (measured). That is why the handshake is given [`Instant::now`] rather
/// than a count of polls.
const HANDSHAKE_POLL_MS: u64 = 10;

/// What the card said, as [`CardSays::as_offset`] encodes it.
///
/// Written by the media task (which owns the card) during
/// [`DOWNLOAD_PREPARING`] and read by the producer. The encoding lives in the
/// download crate, next to its tests.
static DOWNLOAD_FROM: AtomicU32 = AtomicU32::new(0);

/// What the sidecar says a resume should be validated against.
static DOWNLOAD_RESUME_ETAG: CsMutex<RefCell<Option<teddiebox_cloud::ETag>>> =
    CsMutex::new(RefCell::new(None));

/// How many bytes the content file held when the download was planned.
static DOWNLOAD_ON_CARD: AtomicU32 = AtomicU32::new(0);

/// How long the sidecar promised the whole file would be, or zero for none.
///
/// Lets the writer tell a resume of *this* file from one against a file the
/// server has since replaced.
static DOWNLOAD_EXPECT: AtomicU32 = AtomicU32::new(0);

/// Where the server said this body belongs. Zero means it declined the range.
static DOWNLOAD_AT: AtomicU32 = AtomicU32::new(0);

/// How long the server says the whole file is, or zero for "it did not say".
///
/// Left here for the media task, which writes the sidecar but never sees the
/// response headers.
static DOWNLOAD_TOTAL: AtomicU32 = AtomicU32::new(0);

/// What a later resume can be validated against, when the server offered one.
static DOWNLOAD_ETAG: CsMutex<RefCell<Option<teddiebox_cloud::ETag>>> =
    CsMutex::new(RefCell::new(None));
/// Set when the consumer cannot write, so the producer stops rather than
/// wedging against a pipe nobody is draining.
static DOWNLOAD_ABORT: AtomicBool = AtomicBool::new(false);

/// The figure of the fetch that is running, or that most recently ended.
///
/// Set from the [`FetchRequest`] that started it, together with
/// [`DOWNLOAD_DIR`] and [`DOWNLOAD_FILE`]. [`fetch_ended`] reads it, so an
/// outcome is always labelled with the fetch that produced it, not with a
/// newer queued request.
static FETCH_ACTIVE_RUID: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);

/// What the last download came to, for whoever asked for it.
///
/// The fetch runs in the network task and finishes writing in the media task,
/// where the reducer decides what to do about it. Tasks cannot call each
/// other, so the result is left here.
///
/// Only as detailed as the reducer needs; the console prints the exact error.
static FETCH_OUTCOME: AtomicU8 = AtomicU8::new(FETCH_NOTHING);
/// No download has ended since the last one was read.
const FETCH_NOTHING: u8 = 0;
/// The bytes are on the card.
const FETCH_COMPLETED: u8 = 1;
/// The server could not be reached, or could not be asked.
const FETCH_UNREACHABLE: u8 = 2;
/// The server was reached and has nothing filed under that figure.
const FETCH_NO_CONTENT: u8 = 3;
/// The access point turned the box away: the card's passphrase is not its one.
const FETCH_REFUSED: u8 = 4;

/// The byte that carries one of the reducer's reasons between the two tasks.
///
/// An exhaustive match, so a new [`Unavailable`] reason without a byte here
/// fails to compile instead of being reported as the wrong fault.
const fn outcome_for(why: Unavailable) -> u8 {
    match why {
        Unavailable::Unreachable => FETCH_UNREACHABLE,
        Unavailable::Refused => FETCH_REFUSED,
        Unavailable::NoContent => FETCH_NO_CONTENT,
    }
}

/// Which figure [`FETCH_OUTCOME`] belongs to.
///
/// Set together with it, so the media task can tell the result of its own
/// fetch from that of a console `get` that finished while a figure was on the
/// plate.
static FETCH_OUTCOME_RUID: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);

/// Records how a download ended, once.
///
/// A download can end before the request, during the fetch, or at the card;
/// each records its result here, labelled with [`FETCH_ACTIVE_RUID`].
///
/// The outcome and its figure are two atomics, so they are written under one
/// critical section, and [`take_fetch_outcome`] reads them the same way. With
/// one core and no await here nothing could run between the two stores
/// anyway, but the lock makes that explicit and costs little, as it runs once
/// per download.
fn fetch_ended(outcome: u8) {
    critical_section::with(|_| {
        FETCH_OUTCOME_RUID.store(FETCH_ACTIVE_RUID.load(Ordering::Relaxed), Ordering::Relaxed);
        FETCH_OUTCOME.store(outcome, Ordering::Relaxed);
    });
}

/// Records how a download ended, unless something already has.
///
/// Both the net task, which knows *why* a fetch failed, and the media task,
/// which only knows the file stopped short, can report the same fetch. The
/// first report wins, so a precise `NoContent` is not overwritten by
/// `Unreachable`; the box announces those two differently.
///
/// Returns whether it recorded anything. Uses the same critical section as
/// [`fetch_ended`], so the check and the store happen together.
fn fetch_ended_if_silent(outcome: u8) -> bool {
    critical_section::with(|_| {
        if FETCH_OUTCOME.load(Ordering::Relaxed) != FETCH_NOTHING {
            return false;
        }
        FETCH_OUTCOME_RUID.store(FETCH_ACTIVE_RUID.load(Ordering::Relaxed), Ordering::Relaxed);
        FETCH_OUTCOME.store(outcome, Ordering::Relaxed);
        true
    })
}

/// Takes the pending outcome and the figure it belongs to, together.
///
/// `FETCH_NOTHING` means no download has ended since the last read.
fn take_fetch_outcome() -> (u8, u64) {
    critical_section::with(|_| {
        (
            FETCH_OUTCOME.swap(FETCH_NOTHING, Ordering::Relaxed),
            FETCH_OUTCOME_RUID.load(Ordering::Relaxed),
        )
    })
}

/// Records that a probe ended without learning anything.
///
/// **A probe must always answer.** The reducer waits for the answer before
/// playing a placed figure, so a probe that ends silently would leave the box
/// quiet even though the story is on the card. Every net-task path that ends
/// a probe without an answer calls this. Calling it twice is harmless:
/// [`teddiebox_download::Revalidation`] ignores a `Nothing` for a question
/// already settled.
fn probe_ended(ruid: u64) {
    post_answer(ruid, teddiebox_download::Answer::Nothing);
}

/// Turns a failed fetch into one of the reducer's two reasons.
///
/// A `404` or `403` means the server answered but has no story for this
/// figure (teddyCloud answers `403` to a request without a usable token).
/// Anything else (name lookup, socket, handshake, server error) counts as
/// "not reached". The console prints the exact error.
///
/// [`tls::Error::Abandoned`] is handled before this is called, because a
/// transfer that was told to stop is not a failure. If that handling were
/// removed, the catch-all below would report every lifted figure as a
/// network fault.
fn why_unavailable(error: &tls::Error) -> Unavailable {
    match error {
        tls::Error::NoContent
        | tls::Error::Cloud(teddiebox_cloud::CloudError::UnexpectedStatus(403 | 404)) => {
            Unavailable::NoContent
        }
        _ => Unavailable::Unreachable,
    }
}

/// What a probe's answer means for the story on the card.
///
/// Only "the server gave a different length" counts as `Stale`. Everything
/// else answers `Current`, because none of it is evidence against a file that
/// is on the card and plays: checking must never make the box worse than it
/// would be offline.
fn freshness_of(
    answer: teddiebox_download::Answer,
    tag: TagUid,
    card: Option<&storage::Mounted>,
) -> Freshness {
    let teddiebox_download::Answer::Length(total) = answer else {
        return Freshness::Current;
    };
    let Some(card) = card else {
        // Without a card there is nothing to compare against.
        return Freshness::Current;
    };
    let Some(sidecar) = CardIndex::new(card).sidecar(tag) else {
        return Freshness::Current;
    };
    if teddiebox_download::is_stale(&sidecar, teddiebox_cloud::Probed::Length(total)) {
        esp_println::println!(
            "teddiebox: plate {:016X} is {} bytes here and {total} there — fetching it again",
            tag.ruid(),
            sidecar.length
        );
        // Read by the download the reducer is about to request.
        let path = teddiebox_download::content_path(tag.0);
        critical_section::with(|cs| *STALE.borrow_ref_mut(cs) = Some(path));
        Freshness::Stale
    } else {
        Freshness::Current
    }
}

/// Set while audio is being fed, so the download can leave the radio alone.
static PLAYING: AtomicBool = AtomicBool::new(false);

/// How far ahead the download is of whatever is waiting on it.
///
/// The download is not told how far playback has got, so it always reports
/// "nobody is waiting", expressed as a lead that can never be reached. Once
/// playback shares its position, this becomes the written length minus the
/// decoder's position, and the thresholds below take effect.
const NOTHING_WAITING: Pages = Pages(u32::MAX);
/// Fetch again once the decoder is within this many pages of the write head.
const RESUME_BELOW: Pages = Pages(8);
/// Stop once it is this far ahead. The gap between the two keeps the radio
/// from switching on and off for every page the decoder reads.
const PAUSE_ABOVE: Pages = Pages(32);

/// A download being written to the card.
///
/// The card is not stored here. It is passed to `service_download` for one
/// call at a time, so a running download never holds a borrow the media loop
/// needs elsewhere. Only the counters are kept between calls.
struct CacheWrite {
    file: embedded_sdmmc::RawFile,
    writer: Writer,
    crc: teddiebox_core::checksum::Crc32,
}

/// The card end of a download, for as long as one call needs it.
struct CardFile<'a> {
    card: &'a storage::Mounted,
    file: embedded_sdmmc::RawFile,
}

impl ContentSink for CardFile<'_> {
    type Error = &'static str;

    fn append(&mut self, bytes: &[u8]) -> Result<(), &'static str> {
        self.card.append(self.file, bytes)
    }

    fn flush(&mut self) -> Result<(), &'static str> {
        // Printed, not returned. The bytes are on the card and only the
        // recorded length is behind, so a failed flush just means a longer
        // resume later. Returning it would stop a download that can continue.
        if let Err(reason) = self.card.flush(self.file) {
            esp_println::println!("teddiebox: get flush failed — {reason}");
        }
        Ok(())
    }
}

/// How much of a download may be in flight before it is made durable.
///
/// This is what makes an interrupted download resumable: `embedded-sdmmc`
/// only updates a file's recorded length at a flush, so bytes written since
/// the last flush are invisible after a reboot.
///
/// A megabyte is about 24 seconds of download: little to lose, and few enough
/// flushes not to wear the card.
const FLUSH_EVERY: u32 = 1 << 20;

/// Asks the card what it already has, and leaves the answer for the producer.
///
/// Runs in the media task, because only it may touch the card, and *before*
/// the request, because the request depends on what the card holds. With no
/// card it plans a full download, so the failure comes from the write, which
/// reports it clearly.
fn plan_download(card: Option<&storage::Mounted>) -> CardSays {
    let dir = DOWNLOAD_DIR.load(Ordering::Relaxed);
    let file = DOWNLOAD_FILE.load(Ordering::Relaxed);
    critical_section::with(|cs| *DOWNLOAD_RESUME_ETAG.borrow_ref_mut(cs) = None);

    let mut says = CardSays::Nothing;
    let mut on_card = 0;
    let mut expected = 0;

    if let Some(card) = card {
        let length_on_card = card.cache_length(dir, file);
        on_card = length_on_card.unwrap_or(0);

        // **Ignore the sidecar of a file the server found out of date.** It
        // would say the file is complete, and the stale copy would be played
        // again. Without a sidecar the file is fetched again from zero.
        let contradicted = take_stale(dir, file);
        let mut buffer = [0u8; teddiebox_download::MAX_SIDECAR];
        let sidecar = (!contradicted)
            .then(|| {
                card.read_sidecar(dir, file, &mut buffer)
                    .and_then(|filled| core::str::from_utf8(&buffer[..filled]).ok())
                    .and_then(|text| teddiebox_download::Sidecar::parse(text).ok())
            })
            .flatten();

        expected = sidecar.as_ref().map_or(0, |held| held.length);
        let cached = teddiebox_download::Cached {
            sidecar,
            length_on_card,
        };
        match teddiebox_download::decide(&cached) {
            teddiebox_download::Decision::Play => says = CardSays::HoldsAll,
            teddiebox_download::Decision::Fetch => {}
            teddiebox_download::Decision::Resume { from: at, etag } => {
                says = CardSays::Holds(at);
                critical_section::with(|cs| *DOWNLOAD_RESUME_ETAG.borrow_ref_mut(cs) = etag);
            }
        }
    }

    DOWNLOAD_ON_CARD.store(on_card, Ordering::Relaxed);
    DOWNLOAD_EXPECT.store(expected, Ordering::Relaxed);
    says
}

/// Records how long the file beside it is meant to be.
///
/// Best effort. A content file with no sidecar counts as incomplete, so a
/// failure here only costs a refetch later, never a story that stops in the
/// middle. When the server gave no length, no sidecar is written.
fn write_sidecar(card: &storage::Mounted, dir: u32, file: u32) {
    let length = DOWNLOAD_TOTAL.load(Ordering::Relaxed);
    if length == 0 {
        esp_println::println!("teddiebox: get no length from the server — not vouching for it");
        return;
    }
    let etag = critical_section::with(|cs| DOWNLOAD_ETAG.borrow_ref(cs).clone());
    let sidecar = teddiebox_download::Sidecar { length, etag };
    if let Err(reason) = card.write_sidecar(dir, file, sidecar.render().as_bytes()) {
        esp_println::println!("teddiebox: get sidecar not written — {reason}");
    }
}

/// Moves whatever the network has produced onto the card.
///
/// Called from the media task's idle poll, and **never waits**: it writes
/// what is in the pipe and returns, so the media task can still play while a
/// download runs.
///
/// Returns whether a download is still in flight, so the caller can poll
/// faster while one is.
fn service_download(card: Option<&storage::Mounted>, write: &mut Option<CacheWrite>) -> bool {
    let state = DOWNLOAD_STATE.load(Ordering::Relaxed);
    match state {
        DOWNLOAD_IDLE => return false,
        // Both are brief and write nothing: one answers the producer, the
        // other waits for the response headers. Reported as busy so the loop
        // keeps polling fast.
        DOWNLOAD_PREPARING => {
            let says = plan_download(card);
            DOWNLOAD_FROM.store(says.as_offset(), Ordering::Relaxed);
            // Only answers if the producer is still asking. Reading the card
            // can take longer than the producer waits, and an answer stored
            // after it gave up would leave the state stuck in
            // `DOWNLOAD_PLANNED`, with the media loop polling fast for ever.
            // `Handshake::card_answered` applies the same rule at the other
            // end.
            let _ = DOWNLOAD_STATE.compare_exchange(
                DOWNLOAD_PREPARING,
                DOWNLOAD_PLANNED,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
            return true;
        }
        DOWNLOAD_PLANNED => return true,
        _ => {}
    }

    if write.is_none() {
        // A request that failed before the headers arrived sent nothing.
        // Opening the file would create or truncate a cache entry for no
        // reason.
        if state == DOWNLOAD_ENDED && DOWNLOAD_SENT.load(Ordering::Relaxed) == 0 {
            DOWNLOAD_STATE.store(DOWNLOAD_IDLE, Ordering::Relaxed);
            return false;
        }

        let dir = DOWNLOAD_DIR.load(Ordering::Relaxed);
        let file = DOWNLOAD_FILE.load(Ordering::Relaxed);
        // The offset the server sent decides whether the partial file is
        // continued or replaced. Getting it wrong would join the start of a
        // story onto its middle, at a length that looks correct.
        let expected = DOWNLOAD_EXPECT.load(Ordering::Relaxed);
        let total = DOWNLOAD_TOTAL.load(Ordering::Relaxed);
        let placement = teddiebox_download::place(&Landing {
            offset: DOWNLOAD_AT.load(Ordering::Relaxed),
            length_on_card: DOWNLOAD_ON_CARD.load(Ordering::Relaxed),
            // Zero here means "not given", which the crate calls `None`.
            expected: (expected != 0).then_some(expected),
            total: (total != 0).then_some(total),
        });
        let opened = card.ok_or("the card is not mounted").and_then(|card| {
            match placement {
                Placement::Restart => card.open_cache_for_write(dir, file),
                Placement::Continue => card.open_cache_for_append(dir, file),
                Placement::Refuse => Err("the server answered with a range this file cannot take"),
            }
            .map(|handle| (card, handle))
        });
        match opened {
            Ok((card, handle)) => {
                match placement {
                    Placement::Continue => esp_println::println!(
                        "teddiebox: get resuming /CACHE/{dir:08X}/{file:08X} at {}",
                        DOWNLOAD_AT.load(Ordering::Relaxed)
                    ),
                    _ => {
                        esp_println::println!("teddiebox: get writing /CACHE/{dir:08X}/{file:08X}");
                        // Only when starting over. A resumed download
                        // continues the file the existing sidecar describes.
                        write_sidecar(card, dir, file);
                    }
                }
                *write = Some(CacheWrite {
                    file: handle,
                    // From zero even for a resume: it counts what the producer
                    // sends this time, not the length of the file on the card.
                    writer: Writer::resuming(0, FLUSH_EVERY),
                    crc: teddiebox_core::checksum::Crc32::new(),
                });
            }
            Err(reason) => {
                // Abort rather than let the pipe fill, or the producer would
                // wait for ever and the box would look hung.
                esp_println::println!("teddiebox: get cannot write — {reason}");
                // Reported as `Unreachable`: a card problem is not the
                // server's fault, but "reached and has nothing" would be
                // wrong, and `Unreachable` makes the box try again next time.
                fetch_ended(FETCH_UNREACHABLE);
                DOWNLOAD_ABORT.store(true, Ordering::Relaxed);
                DOWNLOAD_STATE.store(DOWNLOAD_IDLE, Ordering::Relaxed);
                return false;
            }
        }
    }

    let (Some(card), Some(active)) = (card, write.as_mut()) else {
        return false;
    };

    let mut sink = CardFile {
        card,
        file: active.file,
    };
    let mut buf = [0u8; 512];
    loop {
        let taken = critical_section::with(|cs| DOWNLOAD_PIPE.borrow_ref_mut(cs).read(&mut buf));
        if taken == 0 {
            break;
        }
        if let Err(reason) = active.writer.write(&mut sink, &buf[..taken]) {
            esp_println::println!("teddiebox: get write failed — {reason}");
            fetch_ended(FETCH_UNREACHABLE);
            DOWNLOAD_ABORT.store(true, Ordering::Relaxed);
            break;
        }
        active.crc.update(&buf[..taken]);
    }

    // Done only when the producer has stopped *and* everything it sent has
    // reached the card; otherwise the file would lose what was still in the
    // pipe.
    let sent = DOWNLOAD_SENT.load(Ordering::Relaxed);
    if state == DOWNLOAD_ENDED && active.writer.watermark() >= Bytes(sent) {
        // Any error was already printed by the sink, and nothing here could
        // act on it.
        let _ = active.writer.finish(&mut sink);
        let written = active.writer.watermark().0;
        esp_println::println!(
            "teddiebox: get wrote {} bytes, crc32 {:08X}",
            written,
            active.crc.finish()
        );
        card.close_file(active.file);
        *write = None;

        // A transfer that stopped is not necessarily a complete story: an
        // aborted or failed download must not be reported as complete, or
        // the reducer would open a fragment. So compare where this body
        // started plus what was written against the length the server gave.
        let whole = teddiebox_download::is_whole(
            DOWNLOAD_AT.load(Ordering::Relaxed),
            written,
            match DOWNLOAD_TOTAL.load(Ordering::Relaxed) {
                0 => None,
                total => Some(total),
            },
        );
        if whole {
            // Reported here, not where the fetch returned: a story is ready
            // once it is on the card, not when the last byte arrived.
            fetch_ended(FETCH_COMPLETED);
        } else {
            esp_println::println!(
                "teddiebox: get stopped short — {} of {} bytes, not vouching for it",
                DOWNLOAD_AT.load(Ordering::Relaxed).saturating_add(written),
                DOWNLOAD_TOTAL.load(Ordering::Relaxed)
            );
            // Not if the download was abandoned: the figure was lifted, and
            // the reducer has already moved on.
            if !DOWNLOAD_ABORT.load(Ordering::Relaxed) {
                fetch_ended_if_silent(FETCH_UNREACHABLE);
            }
        }
        DOWNLOAD_STATE.store(DOWNLOAD_IDLE, Ordering::Relaxed);
        DOWNLOAD_ABORT.store(false, Ordering::Relaxed);
        return false;
    }
    true
}

/// Whether this image has the full console, or only `dl`.
///
/// The console reaches the box over UART0 with no protection beyond opening
/// the case, and some commands can break it: `otaboot` onto a blank slot
/// leaves a box that only a cold boot recovers. That is fine for development
/// but not for a finished box, so release images leave the console out.
///
/// On unless `TEDDIEBOX_RELEASE` is set, so `just firmware` and `just flash`
/// build a development image. `build.rs` turns that variable into the `bench`
/// cfg and rebuilds when it changes. It is an environment variable rather
/// than a Cargo feature, like the language, the SLIX password and the version.
///
/// A `const` rather than a `#[cfg]`, so the dispatch below stays one
/// exhaustive match: a new command must be handled or the build fails. It
/// must be a `const`, not a function: only a constant lets the optimiser drop
/// the code behind it. Measured: gating the whole dispatch this way saves
/// 18,848 bytes, gating eleven commands one by one saved only 3,928, and a
/// helper function made the release build larger than the bench one.
const BENCH: bool = cfg!(bench);

/// Says why a command did nothing, so a release box does not look like it
/// failed to parse the line.
fn not_in_this_build() {
    esp_println::println!(
        "teddiebox: this is a release image — only `dl` is here, to flash a bench one"
    );
}

/// The box's settings: what the card says, and what was typed at the console
/// to override it.
///
/// Anything typed is kept in RAM only and lost at reset, so no credential
/// ends up in the image or the repository. Which source wins is decided in
/// [`teddiebox_config::Settings`], where it is tested; this is only the lock
/// around it.
static SETTINGS: CsMutex<RefCell<Settings>> = CsMutex::new(RefCell::new(Settings::new()));

/// How long to wait for a DHCP lease before calling it a failure.
///
/// Joining the network and getting an address can fail for different
/// reasons, so they are waited for and reported separately.
const DHCP_TIMEOUT: Duration = Duration::from_secs(20);

fn set_ssid(value: String<MAX_SSID>) {
    critical_section::with(|cs| SETTINGS.borrow_ref_mut(cs).set_ssid(value));
}

fn set_password(value: String<MAX_PASSPHRASE>) {
    critical_section::with(|cs| SETTINGS.borrow_ref_mut(cs).set_password(value));
}

/// Publishes what the card said.
///
/// The media task owns the card and calls this once at boot. The console can
/// override it later, so a mistyped card can be worked around without
/// removing it.
fn set_configuration(value: Config) {
    critical_section::with(|cs| SETTINGS.borrow_ref_mut(cs).take_card(value));
}

/// The credentials shaped the way the radio wants them.
///
/// `None` if either the name or the password is missing, because an empty
/// string would fail in a way that looks like a wrong password.
fn credentials() -> Option<Config> {
    critical_section::with(|cs| SETTINGS.borrow_ref(cs).credentials().cloned())
}

fn set_ears_skip(value: bool) {
    critical_section::with(|cs| SETTINGS.borrow_ref_mut(cs).set_ears_skip(value));
}

/// Whether a held ear should skip a chapter, as the card last said.
///
/// Read on each press rather than once, because the card is mounted later:
/// an ear pressed before that would otherwise keep the default for the whole
/// session.
fn ears_skip() -> bool {
    critical_section::with(|cs| SETTINGS.borrow_ref(cs).config().ears_skip)
}

/// Access points one scan will report.
///
/// **16 is a tested limit.** At 48, `scan_async` never returned, on every
/// attempt; the other tasks kept running, so only the radio task was stuck.
/// 16 has always worked. Where the limit lies between the two is unknown;
/// `RADIO_HEAP` is the first suspect.
///
/// The driver fills the list **in channel order, not by signal strength**, so
/// in a busy area it can stop before the high channels (in one test it filled
/// up on channels 1 to 8 and missed a strong access point on channel 11). A
/// cut-off list is therefore reported as cut off.
const SCAN_LIMIT: usize = 16;

/// How long a scan may take before it is called a failure.
///
/// `scan_async` waits for the driver's `ScanDone` event, which a radio that
/// failed to start never sends. `net.rs` has no timeouts of its own, so the
/// console command sets this one.
///
/// **It does not catch everything.** `select` only checks the timer when the
/// task is polled, so a scan that blocks *inside* its own `poll` is never
/// interrupted. That is what happened at `SCAN_LIMIT` 48. This catches a scan
/// that waits for ever for an event, not one that never yields.
const SCAN_TIMEOUT: Duration = Duration::from_secs(15);

/// Set when the box is about to stop being able to write the card.
///
/// The console loop decides to shut down, but the media task owns the card.
/// The shutdown sets this and waits briefly for the media task to clear it
/// after saving the position.
static FLUSH_PLACE: AtomicBool = AtomicBool::new(false);

/// The place a figure was lifted from, before the card knows about it.
///
/// The rules live in [`PendingPlace`], where host tests cover them; this is
/// only the lock around it.
static PENDING_PLACE: CsMutex<RefCell<PendingPlace>> =
    CsMutex::new(RefCell::new(PendingPlace::new()));

/// Where the next story should start.
///
/// Written by `perform` when the reducer decides to play, and taken (cleared)
/// by the media task when it opens the file, so a story started later from
/// the console does not start at the figure's position.
static PLAY_FROM: CsMutex<RefCell<Position>> = CsMutex::new(RefCell::new(Position::Start));

/// The language this box speaks, from `TEDDIEBOX_LANGUAGE` in `.envrc.local`.
///
/// Chosen at build time rather than read from the card, which holds all four
/// languages anyway. Unset means German; an unknown value fails the build,
/// because nobody would notice a box quietly using the wrong language.
const LANGUAGE: Language = match option_env!("TEDDIEBOX_LANGUAGE") {
    Some(name) => match Language::from_name(name) {
        Some(language) => language,
        None => panic!("TEDDIEBOX_LANGUAGE must be one of: de, en-gb, en-us, fr"),
    },
    None => Language::German,
};

/// A sound the box has decided to say about itself, as a file ID.
///
/// `u32::MAX` means nothing pending, since `0` is the start-up jingle and
/// would otherwise be indistinguishable from silence.
static SOUND_REQUEST: AtomicU32 = AtomicU32::new(NO_SOUND);
const NO_SOUND: u32 = u32::MAX;

/// Set while a sound the box asked for is still being played.
///
/// Only cleared by the playback of that sound, which is identified by
/// [`ANNOUNCING_DIRECTORY`] and [`ANNOUNCING_FILE`]. Otherwise a story ending
/// while the "battery critical" message plays would clear it, and the
/// shutdown would cut the message off.
static ANNOUNCING: AtomicBool = AtomicBool::new(false);

/// Set once the power-on jingle has been requested. Once [`ANNOUNCING`] is
/// clear again, the jingle is over and the radio may start without disturbing
/// the sound.
static STARTUP_SOUNDED: AtomicBool = AtomicBool::new(false);

/// Which `CONTENT/<dir>/<file>` [`ANNOUNCING`] is about, so the playback that
/// ends can say whether it is the announcement.
static ANNOUNCING_DIRECTORY: AtomicU32 = AtomicU32::new(0);
static ANNOUNCING_FILE: AtomicU32 = AtomicU32::new(NO_SOUND);

/// Set once the card has been mounted for the first time this boot.
///
/// The card is only mounted when the start-up jingle is played, so this and
/// `CODEC_READY` are what `ota::mark_valid` waits for.
///
/// Setup mode mounts the card for `portal::run` without setting this. That
/// allows testing the revert path without flashing a broken image. The
/// downside: entering setup mode straight after a good update (to fix Wi-Fi
/// credentials, say) leaves the update unconfirmed, and it is reverted on the
/// next boot. Accepted, because setup mode is already a recovery path and the
/// update is simply downloaded again.
static CARD_MOUNTED: AtomicBool = AtomicBool::new(false);

/// Set when the box has said it is turning off, and must therefore do it.
///
/// The `BatteryCritical` sound says "battery is critical, turning off now",
/// so after playing it the box must actually turn off.
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

/// Set once the box has actually parked — rails down, nothing left to say.
///
/// Every periodic task checks this and stops for good, so a parked box does
/// not keep draining the battery with the heartbeat, battery sampling and
/// reducer ticks.
///
/// Set after the card flush and after the power rails go down, because the
/// tasks it stops are the ones that do those.
static PARKED: AtomicBool = AtomicBool::new(false);

/// Set by `awake on`, to keep the box from turning off when idle.
///
/// Reported to the reducer as activity, rather than checked at shutdown, so
/// the reducer stays the only place that decides.
///
/// Off at boot and cleared on every reset.
static STAY_AWAKE: AtomicBool = AtomicBool::new(false);

/// Whether the end of a session is deep sleep rather than a park.
///
/// **On by default.** A sleeping box wakes on an ear press (and reports
/// `rst:0x5 (DSLEEP)`), while a parked box only responds to a power cycle,
/// not even to `dl`.
///
/// Sleep current has not been measured, but a park draws tens of milliamps
/// and sleep does not draw more.
///
/// `autosleep off` makes the box park instead, for one session, so it cannot
/// disappear during a measurement. Reset to on at every boot.
static AUTO_SLEEP: AtomicBool = AtomicBool::new(true);

/// Takes the wake line, arms it, and enters deep sleep. Returns only on
/// failure, and then the box is still awake and still dark.
///
/// The caller has already turned off sound and lights.
async fn sleep_now(lpwr: &mut Option<LPWR<'static>>) -> Result<(), &'static str> {
    inputs::SLEEP_WANTED.store(true, Ordering::Relaxed);
    let mut held = None;
    for _ in 0..40 {
        held = critical_section::with(|cs| inputs::WAKE_LINE.borrow_ref_mut(cs).take());
        if held.is_some() {
            break;
        }
        Timer::after(Duration::from_millis(5)).await;
    }
    let Some(mut wake) = held else {
        return Err("the wake line never arrived");
    };

    // A held ear keeps the line at its wake level, which would end the sleep
    // at once, so wait for it to be released. Limited to ten seconds; longer
    // is a fault.
    for _ in 0..200 {
        if wake.is_high() {
            break;
        }
        Timer::after(Duration::from_millis(50)).await;
    }

    // Printed before arming: the message needs time to leave the UART, and
    // an ear press during that wait must not happen while the wake is armed.
    esp_println::println!("teddiebox: sleeping — press an ear to wake");
    Timer::after(Duration::from_millis(50)).await;

    let outcome = sleep::arm(&mut wake).and_then(|()| {
        lpwr.take()
            .ok_or("the low-power peripheral has already been taken")
    });
    match outcome {
        Ok(lpwr) => sleep::enter(lpwr),
        Err(reason) => {
            critical_section::with(|cs| {
                inputs::WAKE_LINE.borrow_ref_mut(cs).replace(wake);
            });
            Err(reason)
        }
    }
}

/// Everything a child can see or hear, off, in the order the hardware needs.
///
/// Shared by the box turning itself off and the console asking it to, so the
/// order is only written once.
async fn go_dark(board: &mut BoardPins<'_>, gates: &mut Gates, rgb: Option<&led::Rgb<'_>>) {
    i2c_bus::quieten_codec().await;
    // Before the rail goes down, not after. LEDC keeps driving the LED on its
    // own, and `Gates::led` refuses once the peripherals rail is down, so
    // doing this second would leave the LED lit.
    if let Some(rgb) = rgb {
        if let Ok(dark) = gates.led(board::Colour::Off) {
            rgb.apply(&dark, board::LED_DUTY);
        }
    }
    board.apply_all(&gates.release_for_reset());
    drain_console();
}

/// Stops the calling task for the rest of this power-on.
///
/// `pending` never completes, so the task is never scheduled again. Not a
/// `return`, which would drop the pins and buses the task owns and might
/// change their state; this keeps every pin as the park left it.
async fn park_task() {
    core::future::pending::<()>().await;
}

/// Which content file `play` names, as two halves of `CONTENT/<dir>/<file>`.
static CONTENT_DIRECTORY: AtomicU32 = AtomicU32::new(0);
static CONTENT_FILE: AtomicU32 = AtomicU32::new(0);

/// How many frames `pcm` should print.
static PCM_FRAMES: AtomicU8 = AtomicU8::new(0);

/// How much room the card's configuration file is given.
///
/// A file that fills this is refused rather than parsed (see
/// [`teddiebox_config::Config::parse_read`]), so this is the file-size limit.
/// A typical file is under a kilobyte; the rest leaves room for comments.
const CONFIG_BUFFER: usize = 2048;

/// Reads the card's copy of the certificate authority, so the server can be
/// verified even on a box whose identity is missing or invalid.
///
/// The box's own certificate and key are never read from the card: they come
/// from flash, read once at boot in [`identity::load`].
fn read_anchor(card: &storage::Mounted) {
    let mut certificate = [0u8; tls::CERT_BYTES];

    // Printed as `ca`, not `identity`, which is used for the box's own
    // certificate and key, so the two are not confused.
    match card.read_certificate("TCCA.DER", &mut certificate) {
        Ok(n) if tls::set_anchor(&certificate[..n]) => {
            esp_println::println!("teddiebox: ca {n} bytes — the server is checked against it")
        }
        Ok(_) => esp_println::println!("teddiebox: ca already set"),
        Err(reason) => esp_println::println!(
            "teddiebox: ca none — {reason}; the server cannot be verified, \
             so every download will fail until TCCA.DER is on the card"
        ),
    }
}

/// Reads `CONFIG.TXT` the first time the card is available, and says what it
/// found.
///
/// **A bad configuration file does not stop the box.** Everything on the card
/// still plays; only the network stays down.
///
/// The problem is reported both on the console and as a spoken sound, for
/// people without a cable.
fn read_configuration_once(card: &storage::Mounted, done: &mut bool) {
    if *done {
        return;
    }
    *done = true;

    read_anchor(card);

    let mut buffer = [0u8; CONFIG_BUFFER];
    match card.read_config(&mut buffer) {
        Ok(config) => {
            // The passphrase is never printed; its length is enough to spot
            // a truncated one.
            esp_println::println!(
                "teddiebox: config ssid {}, server {}, passphrase {} chars",
                config.ssid,
                config.server,
                config.password.len()
            );
            set_configuration(config);
        }
        Err(trouble) => {
            esp_println::println!(
                "teddiebox: config unusable — {trouble:?}. The network stays down; \
                 everything on the card still plays."
            );
            SOUND_REQUEST.store(Sound::ConfigError.file(), Ordering::Relaxed);
        }
    }
}

/// Holds a figure's place in RAM, writing the previous one out if it must.
///
/// Only the displaced figure's place is written to the card, because it is
/// about to be dropped from RAM.
fn remember_in_ram(index: &CardIndex<'_>, tag: TagUid, page: u32) {
    let displaced =
        critical_section::with(|cs| PENDING_PLACE.borrow_ref_mut(cs).remember(tag, page));
    if let Some((held, held_page)) = displaced {
        write_place(index, held, held_page, "making room");
    }
    esp_println::println!(
        "teddiebox: plate holding {:016X} at page {page}",
        tag.ruid()
    );
}

/// What RAM is holding for this figure, if anything.
///
/// Checked before the card, because it is always newer: it is written as soon
/// as a figure is lifted, and the card only later.
pub(crate) fn held_place(tag: TagUid) -> Option<u32> {
    critical_section::with(|cs| PENDING_PLACE.borrow_ref(cs).held(tag))
}

/// Drops a story's held place, by the path it lives at.
///
/// Called when a story reaches its end. Takes the ruid rather than the tag,
/// because the media task knows the file it plays, not which figure asked.
fn forget_place(ruid: u64) {
    critical_section::with(|cs| PENDING_PLACE.borrow_ref_mut(cs).forget(ruid));
}

/// Puts whatever RAM is holding onto the card.
///
/// Called when RAM is about to be lost. The slot is emptied, so a second call
/// writes nothing.
fn flush_place(index: &CardIndex<'_>, why: &str) {
    let held = critical_section::with(|cs| PENDING_PLACE.borrow_ref_mut(cs).take());
    if let Some((tag, page)) = held {
        write_place(index, tag, page, why);
    }
}

fn write_place(index: &CardIndex<'_>, tag: TagUid, page: u32, why: &str) {
    match index.remember(tag, page) {
        Ok(()) => esp_println::println!(
            "teddiebox: plate wrote {:016X} at page {page} — {why}",
            tag.ruid()
        ),
        Err(reason) => esp_println::println!("teddiebox: plate place not saved — {reason}"),
    }
}

/// Carries out one of the reducer's decisions.
///
/// Most actions set the same statics as the matching console command.
///
/// `token` is the credential of the figure currently on the plate, passed in
/// rather than kept in a shared static; see [`FetchRequest`] for why.
fn perform(action: Action, index: &CardIndex<'_>, token: Option<[u8; 32]>) {
    match action {
        Action::Play { tag, from } => {
            let path = teddiebox_download::content_path(tag.0);
            // A story that came with the card is under `CONTENT/`, a
            // downloaded one under `CACHE/`, and each request only looks in
            // its own directory.
            let request = if index.on_stock_card(path.directory, path.file) {
                REQUEST_CONTENT
            } else {
                REQUEST_CACHE
            };
            critical_section::with(|cs| *PLAY_FROM.borrow_ref_mut(cs) = from);
            esp_println::println!(
                "teddiebox: plate playing {}/{:08X}/{:08X}",
                if request == REQUEST_CONTENT {
                    "CONTENT"
                } else {
                    "CACHE"
                },
                path.directory,
                path.file
            );
            CONTENT_DIRECTORY.store(path.directory, Ordering::Relaxed);
            CONTENT_FILE.store(path.file, Ordering::Relaxed);
            // The storage rail is already on, or the card would not be
            // mounted. The output is asked for here, ahead of the request, so
            // its power-up overlaps the card's first reads.
            i2c_bus::request_output(i2c_bus::OUTPUT_UP);
            REQUEST.store(request, Ordering::Relaxed);
        }

        // Both just stop playback. Where the story got to is saved separately,
        // by `SavePosition`.
        Action::Pause | Action::Stop => {
            esp_println::println!("teddiebox: plate stopping");
            audio::STOP.store(true, Ordering::Relaxed);
        }

        // Limits and steps are handled by the reducer; this only passes on
        // the level in dB.
        Action::SetVolume { step, db } => {
            esp_println::println!("teddiebox: volume step {} — {db} dB", step.0);
            i2c_bus::VOLUME_REQUEST.store(db, Ordering::Relaxed);
        }

        // Only mutes or unmutes the class-D driver; `HEADPHONES_IN` is left
        // to the detect poll. Muted rather than powered down, because
        // powering an output stage clicks even when it is muted.
        Action::SetOutput(output) => {
            esp_println::println!(
                "teddiebox: output {}",
                match output {
                    AudioOutput::Speaker => "speaker",
                    AudioOutput::Headphones => "headphones",
                }
            );
            // `SPEAKER_RESUME` rather than a plain unmute: a story started
            // with headphones in never powered the amplifier. The click of
            // powering it is acceptable just after a plug was pulled.
            i2c_bus::SPEAKER_REQUEST.store(
                match output {
                    AudioOutput::Speaker => i2c_bus::SPEAKER_RESUME,
                    AudioOutput::Headphones => i2c_bus::SPEAKER_MUTE,
                },
                Ordering::Relaxed,
            );
        }

        // Only the direction is given; the decoder knows the chapters and
        // what to do at either end of the story.
        Action::NextTrack => {
            esp_println::println!("teddiebox: skip forward");
            audio::SKIP.store(audio::SKIP_FORWARD, Ordering::Relaxed);
        }

        Action::PrevTrack => {
            esp_println::println!("teddiebox: skip back");
            audio::SKIP.store(audio::SKIP_BACK, Ordering::Relaxed);
        }

        Action::AbortFetch => {
            esp_println::println!("teddiebox: plate abandoning the download");
            DOWNLOAD_ABORT.store(true, Ordering::Relaxed);
        }

        Action::RequestContent(tag) => {
            let ruid = tag.ruid();
            esp_println::println!("teddiebox: plate fetching {ruid:016X}");
            // Written as one value; see `FetchRequest`.
            critical_section::with(|cs| {
                *FETCH_REQUEST.borrow_ref_mut(cs) = Some(FetchRequest {
                    ruid,
                    token,
                    probe: false,
                });
            });
            NET_REQUEST.store(NET_GET, Ordering::Relaxed);
        }

        Action::Revalidate(tag) => {
            let ruid = tag.ruid();
            esp_println::println!("teddiebox: plate asking the server about {ruid:016X}");
            critical_section::with(|cs| {
                REVALIDATION
                    .borrow_ref_mut(cs)
                    .asked(ruid, Instant::now().as_millis())
            });
            critical_section::with(|cs| {
                *FETCH_REQUEST.borrow_ref_mut(cs) = Some(FetchRequest {
                    ruid,
                    token,
                    probe: true,
                });
            });
            NET_REQUEST.store(NET_GET, Ordering::Relaxed);
        }

        Action::PlayCue(cue) => audio::CUE.store(cue.code(), Ordering::Relaxed),

        Action::PlayPrompt(prompt) => {
            SOUND_REQUEST.store(Sound::for_prompt(prompt).file(), Ordering::Relaxed)
        }

        Action::SavePosition { tag, pos } => {
            // Kept in RAM, not written to the card: figures are lifted and put
            // back often, and the card only needs the place when RAM is about
            // to be lost.
            if let Position::Exact { page } = pos {
                remember_in_ram(index, tag, page);
            }
        }

        Action::SetLed(state) => LED_REQUEST.store(state.code(), Ordering::Relaxed),

        // The only place the box decides to turn off. It is only flagged
        // here: the console loop owns the rails, the codec and the last card
        // write, and waits for the announcement to finish before acting. The
        // reason is printed here, because only this place knows it.
        Action::PowerOff(reason) => {
            esp_println::println!(
                "teddiebox: plate powering off — {}",
                match reason {
                    PowerOffReason::PackEmpty => "the pack is below the cutoff",
                    PowerOffReason::Idle => "nothing has used the box",
                }
            );
            // Stop the story: the cells have no protection circuit, and a
            // story left running would drain them for another half hour. The
            // announcement still plays, because every playback clears `STOP`
            // before its first frame.
            //
            // A stop is not an ending: `play_taf` returns `Finish::Stopped`,
            // and only `Finish::Ended` clears the story's place, so the place
            // survives the power-off.
            audio::STOP.store(true, Ordering::Relaxed);
            SHUTTING_DOWN.store(true, Ordering::Relaxed);
        }

        // Any other action is not handled; print it.
        other => esp_println::println!("teddiebox: plate not wired — {other:?}"),
    }
}

/// Drains the reader's signal into what the media task believes is on the
/// plate.
///
/// The figure and its token are updated together by [`Placed::observe`],
/// which has host tests; only the signal is handled here.
///
/// Called once per pass of the media loop and once per frame while a story
/// plays, because one loop pass lasts a whole story and a lift must be seen
/// before it ends.
fn take_plate_event(placed: &mut Placed) -> Option<Event> {
    let event = placed.observe(nfc::PLATE_TAG.try_take()?);
    // A lift cancels any open question about the figure, as soon as it is
    // seen. Later in the pass, the figure might already be back on the plate.
    if event == Event::TagAbsent {
        critical_section::with(|cs| REVALIDATION.borrow_ref_mut(cs).withdrawn());
    }
    Some(event)
}

/// Hands the reducer every ear edge that has settled since it was last asked.
///
/// Drains the whole queue, because `Core` pairs a press with its release, and
/// leaving half a pair behind would turn the next tap into a hold.
///
/// Needs the card only because `Core::handle` takes an index for every event.
/// Without a card there is nothing to play, so ears not working then does
/// not matter.
fn apply_ear_events(reducer: &mut Core, card: &storage::Mounted, token: Option<[u8; 32]>) {
    while let Ok(event) = inputs::INPUT_EVENTS.try_receive() {
        apply(reducer, card, event, token);
    }
}

/// Tells the reducer what happened and carries out what it decides.
///
/// The index is built here and dropped at the end, because it borrows the
/// card, which the rest of the media loop needs. It is cheap to build.
fn apply(reducer: &mut Core, card: &storage::Mounted, event: Event, token: Option<[u8; 32]>) {
    // Updated on every event, not once a second with the tick, so a press
    // right after `ears skip off` already uses the new setting. It is cheap.
    reducer.note_ears_skip(ears_skip());
    let index = CardIndex::new(card);
    for action in reducer.handle(event, &index) {
        perform(action, &index, token);
    }
}

/// The core's only clock, fed at most once a second.
///
/// Called from the top of the media loop and from the per-frame `attend`
/// closure while a story plays, so it limits itself: a tick every frame would
/// add enough work to cause audible DMA restarts. Once a second is plenty for
/// a five-minute timeout.
fn feed_tick(
    reducer: &mut Core,
    card: &storage::Mounted,
    token: Option<[u8; 32]>,
    last_fed: &mut u64,
) {
    let now = Instant::now().as_millis();
    if now.saturating_sub(*last_fed) >= 1_000 {
        *last_fed = now;
        // The reducer only knows about figures on the plate. Console
        // playback, a `batlog` run or `awake on` also count as use, or the
        // idle timeout would cut them short.
        reducer.note_in_use(
            PLAYING.load(Ordering::Relaxed)
                || battery::BATLOG_EVERY.load(Ordering::Relaxed) > 0
                || STAY_AWAKE.load(Ordering::Relaxed),
        );
        apply(reducer, card, Event::Tick(now), token);
    }
}

/// How long the codec's output stays powered after the last sound.
///
/// Powering it up clicks and takes a moment, so a run of presses, or a press
/// just after a story, would otherwise click each time.
const OUTPUT_LINGER: Duration = Duration::from_secs(5);

/// How long an idle cue waits for the output to come up before giving up.
const OUTPUT_UP_WAIT: Duration = Duration::from_secs(1);

/// Plays a cue while nothing else holds the output, powering it first.
///
/// Returns whether it played.
async fn play_idle_cue(
    cue: Cue,
    i2s_tx: &mut Option<esp_hal::i2s::master::I2sTx<'static, esp_hal::Blocking>>,
    wav_buffer: &mut Option<esp_hal::dma::DmaTxStreamBuf>,
) -> bool {
    // Checked before taking either: taking both and then matching would drop
    // one if the other was missing.
    if i2s_tx.is_none() || wav_buffer.is_none() {
        esp_println::println!("teddiebox: cue {cue:?} — the audio hardware is claimed");
        return false;
    }
    // Asked for even when the output reads as up: it replaces a power-down
    // the idle loop may have queued a moment ago, and costs nothing when the
    // output really is up.
    i2c_bus::request_output(i2c_bus::OUTPUT_UP);
    if !i2c_bus::OUTPUT_IS_UP.load(Ordering::Relaxed) {
        let asked = Instant::now();
        while !i2c_bus::OUTPUT_IS_UP.load(Ordering::Relaxed) {
            if asked.elapsed() >= OUTPUT_UP_WAIT {
                esp_println::println!("teddiebox: cue {cue:?} — the output did not come up");
                return false;
            }
            Timer::after(Duration::from_millis(5)).await;
        }
        esp_println::println!(
            "teddiebox: cue output up in {} ms",
            asked.elapsed().as_millis()
        );
    }
    let (Some(tx), Some(buffer)) = (i2s_tx.take(), wav_buffer.take()) else {
        return false;
    };
    let (result, tx, buffer) = audio::play_cue(tx, buffer, cue).await;
    *i2s_tx = Some(tx);
    *wav_buffer = Some(buffer);
    match result {
        Ok(()) => true,
        Err(reason) => {
            esp_println::println!("teddiebox: cue {cue:?} failed — {reason}");
            false
        }
    }
}

/// Owns the I2S peripheral and the SD card, runs the reducer, plays stories
/// and sounds, writes downloads to the card, and serves the console commands
/// that need this hardware.
///
/// The card is mounted once and kept. The test tone and WAV playback keep the
/// I2S peripheral until the box restarts; story playback gives it back when
/// it ends.
#[embassy_executor::task]
async fn media(
    spi: Spi<'static, esp_hal::Blocking>,
    cs: Output<'static>,
    i2s_tx: esp_hal::i2s::master::I2sTx<'static, esp_hal::Blocking>,
    mut tone_buffer: esp_hal::dma::DmaLoopBuf,
    wav_buffer: esp_hal::dma::DmaTxStreamBuf,
) {
    // Exactly one cycle of a sine, repeated by the DMA for ever. A loop
    // buffer, not a stream buffer: a stream transfer starts at once and
    // underruns before the first sample is written.
    for (i, sample) in tone::SINE.iter().enumerate() {
        let bytes = sample.to_le_bytes();
        // The same sample in both stereo channels: the speaker uses the left
        // channel and the headphones use both.
        tone_buffer[i * 4] = bytes[0];
        tone_buffer[i * 4 + 1] = bytes[1];
        tone_buffer[i * 4 + 2] = bytes[0];
        tone_buffer[i * 4 + 3] = bytes[1];
    }

    // Each is held in an Option and taken when a command needs it; the tone
    // and WAV playback never give theirs back.
    let mut card: Option<storage::Mounted> = None;
    // The configuration is read from the card only once, so a later read
    // does not undo settings typed at the console.
    let mut configured = false;
    let mut bus = Some((spi, cs));
    let mut i2s_tx = Some(i2s_tx);
    let mut tone_buffer = Some(tone_buffer);
    let mut wav_buffer = Some(wav_buffer);
    // When the last sound ended, while the output is still powered; the idle
    // loop powers it down `OUTPUT_LINGER` later.
    let mut quiet_since: Option<Instant> = None;
    // The tone stops when its transfer is dropped, so it is kept here until
    // the box restarts.
    let mut _tone_transfer = None;

    let mut download: Option<CacheWrite> = None;

    // Decides what to do with a figure on the plate. It lives here because it
    // needs to know what is on the card, and this task owns the card.
    let mut reducer = Core::new(CoreConfig::default());
    // The figure on the plate, if any, with its token. Used to match a
    // finished download or probe to the figure it was for; with no figure, a
    // late result is ignored. The token is kept here rather than in a shared
    // static, so nothing typed at the console can be attached to this
    // figure's fetch.
    let mut placed = Placed::empty();
    // When a `Tick` was last fed to the reducer, shared by both `feed_tick`
    // call sites.
    let mut last_tick_fed: u64 = 0;
    // Whether this boot has asked the net task to prime the connection (see
    // `NET_PRIME`). Only tried once per boot.
    let mut primed = false;

    loop {
        // Nothing to do for the rest of this power-on: see `PARKED`.
        if PARKED.load(Ordering::Relaxed) {
            park_task().await;
        }

        // Events are handled before the request is read, so a `Play` decided
        // here starts in the same pass.
        let mut events: [Option<Event>; 3] = [None, None, None];

        // Checked first, because the card is about to become unwritable.
        if FLUSH_PLACE.swap(false, Ordering::Relaxed) {
            if let Some(card) = card.as_ref() {
                flush_place(&CardIndex::new(card), "the box is stopping");
            }
        }

        events[0] = take_plate_event(&mut placed);

        // A download ended in another task. Read every pass so an old result
        // cannot later be matched to a different figure.
        let (outcome, outcome_ruid) = take_fetch_outcome();
        if outcome != FETCH_NOTHING {
            match placed.answering(outcome_ruid) {
                // The figure it was for is still on the plate. (The reducer
                // checks the identity again before acting.)
                Answering::TheFigure(tag) => {
                    events[1] = match outcome {
                        FETCH_COMPLETED => Some(Event::ContentReady(tag)),
                        FETCH_UNREACHABLE => {
                            Some(Event::ContentMissing(tag, Unavailable::Unreachable))
                        }
                        FETCH_NO_CONTENT => {
                            Some(Event::ContentMissing(tag, Unavailable::NoContent))
                        }
                        FETCH_REFUSED => Some(Event::ContentMissing(tag, Unavailable::Refused)),
                        _ => unreachable!("outcome != FETCH_NOTHING was just checked"),
                    };
                }
                // A different figure is on the plate, for example after a
                // console `get`. Not passed to the reducer.
                Answering::AnotherFigure => esp_println::println!(
                    "teddiebox: plate ignoring a fetch outcome for {outcome_ruid:016X} — \
                     not the figure on the plate"
                ),
                // Nobody is waiting; nothing to do.
                Answering::NoFigure => {}
            }
        }

        // The server has answered a question about a cached story. Read every
        // pass and matched to the figure, like the fetch outcome. Each call is
        // its own short critical section, with no printing inside: a console
        // line at 115200 baud would keep interrupts off for milliseconds.
        let mut settled = None;
        if let Some((ruid, answer)) = take_answer() {
            settled =
                critical_section::with(|cs| REVALIDATION.borrow_ref_mut(cs).answered(ruid, answer));
            if settled.is_none() {
                // The question is already over: it timed out, or an earlier
                // answer settled it. Acting on it could mark a story stale
                // that is already playing, so it is only printed.
                esp_println::println!("teddiebox: plate ignoring a late answer about {ruid:016X}");
            }
        }
        if settled.is_none() {
            settled = critical_section::with(|cs| {
                REVALIDATION
                    .borrow_ref_mut(cs)
                    .polled(Instant::now().as_millis())
            });
            if let Some(teddiebox_download::Settled { ruid, .. }) = settled {
                esp_println::println!(
                    "teddiebox: plate {ruid:016X} — no answer in {} s, playing what is here",
                    teddiebox_download::PATIENCE_MS / 1_000
                );
            }
        }
        if let Some(teddiebox_download::Settled {
            ruid: probed_ruid,
            answer,
        }) = settled
        {
            match placed.answering(probed_ruid) {
                Answering::TheFigure(tag) => {
                    // Remembered whatever the answer: a server that was just
                    // unreachable will likely stay so for the session, and
                    // asking again wastes the radio.
                    critical_section::with(|cs| ASKED.borrow_ref_mut(cs).remember(probed_ruid));
                    events[2] = Some(Event::Revalidated(
                        tag,
                        freshness_of(answer, tag, card.as_ref()),
                    ));
                }
                Answering::AnotherFigure => esp_println::println!(
                    "teddiebox: plate ignoring an answer about {probed_ruid:016X} — \
                     not the figure on the plate"
                ),
                Answering::NoFigure => {}
            }
        }

        // Once the jingle is over and the plate is empty, do the slow part of
        // the first network request in advance. A figure already on the plate
        // does this work itself, and one placed during priming uses the same
        // connection.
        if !primed
            && configured
            && placed.figure().is_none()
            && STARTUP_SOUNDED.load(Ordering::Relaxed)
            && !ANNOUNCING.load(Ordering::Relaxed)
            && !PLAYING.load(Ordering::Relaxed)
        {
            primed = true;
            if battery::pack_too_low_to_prime() {
                esp_println::println!("teddiebox: net not priming — the pack is low");
            } else if NET_REQUEST
                .compare_exchange(
                    REQUEST_NONE,
                    NET_PRIME,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                esp_println::println!("teddiebox: net priming the connection for the first figure");
            }
        }

        // Every pass, not only when a figure moved: ear presses are
        // independent of the plate.
        if let Some(mounted) = card.as_ref() {
            apply_ear_events(&mut reducer, mounted, placed.token());
        }

        // The reducer's only clock; it turns the box off after the idle
        // timeout. A story plays inside `play_taf` below, for up to half an
        // hour, so `feed_tick` is also called from its `attend` closure.
        if let Some(mounted) = card.as_ref() {
            feed_tick(&mut reducer, mounted, placed.token(), &mut last_tick_fed);
        }

        if events.iter().any(Option::is_some) {
            match card.as_ref() {
                Some(mounted) => {
                    for event in events.into_iter().flatten() {
                        apply(&mut reducer, mounted, event, placed.token());
                    }
                }
                // Without a card the figure cannot be handled; at least say
                // so on the console.
                None => esp_println::println!(
                    "teddiebox: plate no card mounted — the figure cannot be answered"
                ),
            }
        }

        let request = REQUEST.swap(REQUEST_NONE, Ordering::Relaxed);
        if request == REQUEST_NONE {
            if let Some(cue) = audio::take_cue() {
                // Only a cue that played leaves the output up to be powered
                // down later; one that could not play (the console tone holds
                // the hardware) must not power that tone off.
                if play_idle_cue(cue, &mut i2s_tx, &mut wav_buffer).await {
                    quiet_since = Some(Instant::now());
                }
                continue;
            }
            if quiet_since.is_some_and(|since| since.elapsed() >= OUTPUT_LINGER)
                && REQUEST.load(Ordering::Relaxed) == REQUEST_NONE
            {
                i2c_bus::request_output(i2c_bus::OUTPUT_DOWN);
                quiet_since = None;
            }
            // Serviced when idle, so a download progresses in the gaps without
            // blocking the loop.
            let downloading = service_download(card.as_ref(), &mut download);
            // A late drain stalls the socket, so poll fast while downloading.
            let gap = if downloading { 5 } else { 100 };
            Timer::after(Duration::from_millis(gap)).await;
            continue;
        }

        // The console loop switched the storage rail on before setting the
        // request; wait briefly for the supply to settle before using the
        // card.
        if matches!(
            request,
            REQUEST_WALK
                | REQUEST_WAV
                | REQUEST_TAF
                | REQUEST_CONTENT
                | REQUEST_CACHE
                | REQUEST_PCM
                | REQUEST_CRC
        ) && card.is_none()
        {
            let Some((spi, cs)) = bus.take() else {
                esp_println::println!("teddiebox: the card bus is gone — reboot to retry");
                continue;
            };
            Timer::after(Duration::from_millis(50)).await;
            match storage::Mounted::open(spi, cs, esp_hal::delay::Delay::new()) {
                Ok(mounted) => {
                    read_configuration_once(&mounted, &mut configured);
                    CARD_MOUNTED.store(true, Ordering::Relaxed);
                    card = Some(mounted)
                }
                Err(reason) => {
                    // The attempt used up the bus, so the card cannot be
                    // retried without a restart.
                    esp_println::println!("teddiebox: sd failed — {reason}");
                    continue;
                }
            }
        }

        match request {
            REQUEST_WALK => {
                if let Some(card) = card.as_ref() {
                    card.walk().await;
                }
            }

            REQUEST_TONE => {
                // The tone plays until a restart, so nothing may power its
                // output down.
                quiet_since = None;
                let (Some(tx), Some(buffer)) = (i2s_tx.take(), tone_buffer.take()) else {
                    esp_println::println!("teddiebox: I2S is already in use");
                    continue;
                };
                match tx.write(buffer) {
                    Ok(transfer) => {
                        esp_println::println!(
                            "teddiebox: playing {} Hz at {} Hz",
                            tone::TONE_HZ,
                            tone::SAMPLE_RATE_HZ
                        );
                        _tone_transfer = Some(transfer);
                    }
                    Err(_) => esp_println::println!("teddiebox: I2S would not start"),
                }
            }

            REQUEST_PCM => {
                if let Some(card) = card.as_ref() {
                    let frames = PCM_FRAMES.load(Ordering::Relaxed);
                    if let Err(reason) = audio::dump_pcm(card, frames).await {
                        esp_println::println!("teddiebox: pcm failed — {reason}");
                    }
                }
            }

            REQUEST_CRC => {
                if let Some(card) = card.as_ref() {
                    let directory = CONTENT_DIRECTORY.load(Ordering::Relaxed);
                    let file = CONTENT_FILE.load(Ordering::Relaxed);
                    match card.checksum_cache(directory, file).await {
                        Ok((size, crc)) => esp_println::println!(
                            "teddiebox: crc /CACHE/{directory:08X}/{file:08X} {size} {crc:08X}"
                        ),
                        Err(reason) => esp_println::println!(
                            "teddiebox: crc /CACHE/{directory:08X}/{file:08X} failed — {reason}"
                        ),
                    }
                }
            }

            REQUEST_WAV | REQUEST_TAF | REQUEST_CONTENT | REQUEST_CACHE => {
                let Some(card) = card.as_ref() else {
                    continue;
                };
                // Checked before taking either: taking both and then matching
                // would drop one if the other was missing.
                if i2s_tx.is_none() || wav_buffer.is_none() {
                    esp_println::println!(
                        "teddiebox: the audio hardware is already claimed — rb to run another"
                    );
                    continue;
                }
                let (Some(tx), Some(buffer)) = (i2s_tx.take(), wav_buffer.take()) else {
                    continue;
                };

                // Clear any stop, skip or cue left over from before, or it
                // would act on this playback.
                audio::STOP.store(false, Ordering::Relaxed);
                audio::SKIP.store(audio::SKIP_NONE, Ordering::Relaxed);
                audio::CUE.store(audio::CUE_NONE, Ordering::Relaxed);
                // The idle loop may have asked for a power-down in the moment
                // before this request arrived; this undoes it. Harmless when
                // the output is already up.
                i2c_bus::request_output(i2c_bus::OUTPUT_UP);
                quiet_since = None;

                // Set while feeding the DMA, so the download leaves the radio
                // alone. Always cleared afterwards, even on failure, or the
                // download would pause for ever.
                PLAYING.store(true, Ordering::Relaxed);
                if request == REQUEST_WAV {
                    if let Err(reason) = audio::play_first_wav(card, tx, buffer).await {
                        esp_println::println!("teddiebox: playback failed — {reason}");
                    }
                } else {
                    let source = match request {
                        REQUEST_CONTENT => audio::Source::Content {
                            directory: CONTENT_DIRECTORY.load(Ordering::Relaxed),
                            file: CONTENT_FILE.load(Ordering::Relaxed),
                        },
                        REQUEST_CACHE => audio::Source::Cache {
                            directory: CONTENT_DIRECTORY.load(Ordering::Relaxed),
                            file: CONTENT_FILE.load(Ordering::Relaxed),
                        },
                        _ => audio::Source::First,
                    };
                    // Only a story with a known path has a position to keep.
                    // A console `taf` plays `Source::First`, and
                    // `CONTENT_DIRECTORY` then still names the previous story,
                    // so without this check its position would be overwritten.
                    let identified = matches!(request, REQUEST_CONTENT | REQUEST_CACHE);
                    let stock = request == REQUEST_CONTENT;
                    let dir = CONTENT_DIRECTORY.load(Ordering::Relaxed);
                    let file = CONTENT_FILE.load(Ordering::Relaxed);
                    let from = match critical_section::with(|cs| {
                        core::mem::replace(&mut *PLAY_FROM.borrow_ref_mut(cs), Position::Start)
                    }) {
                        // Cleared either way, so the next story does not
                        // start at this figure's position.
                        from if identified => from,
                        _ => Position::Start,
                    };
                    // Called on every frame, because a story is one pass of
                    // this loop and can last half an hour: lifting the figure,
                    // ear presses and the idle clock must still be handled.
                    let mut attend = || {
                        if let Some(event) = take_plate_event(&mut placed) {
                            apply(&mut reducer, card, event, placed.token());
                        }
                        apply_ear_events(&mut reducer, card, placed.token());
                        feed_tick(&mut reducer, card, placed.token(), &mut last_tick_fed);

                        // Keeps the reducer's position current; it is what
                        // gets saved when the figure is lifted.
                        reducer.note_position(Position::Exact {
                            page: audio::PAGE.load(Ordering::Relaxed),
                        });
                    };
                    // The hardware is returned, so another story can play
                    // without a reboot.
                    let (outcome, tx, buffer) =
                        audio::play_taf(card, tx, buffer, source, from, &mut attend).await;
                    if let Err(reason) = outcome {
                        esp_println::println!("teddiebox: playback failed — {reason}");
                    }
                    if matches!(outcome, Ok(audio::Finish::Ended)) {
                        if identified {
                            // A finished story starts again from the
                            // beginning next time. Written as page 0 rather
                            // than deleted: `storage` cannot delete, and page 0
                            // is the header, so it means the same as no file.
                            let mut out = [0u8; MAX_POSITION];
                            let len = position::render(0, &mut out);
                            let _ = card.write_position(stock, dir, file, &out[..len]);
                            // Also drop the RAM copy, which would otherwise
                            // win over the card.
                            forget_place(((dir as u64) << 32) | file as u64);
                        }
                        // The reducer cannot learn this any other way. Sent
                        // for every story that ends, including console
                        // playback, so the reducer returns to idle.
                        apply(&mut reducer, card, Event::PlaybackEnded, placed.token());
                    }
                    // Cleared only by the announcement's own playback, but
                    // whatever the outcome, or a failed announcement would
                    // block the shutdown for ever.
                    if dir == ANNOUNCING_DIRECTORY.load(Ordering::Relaxed)
                        && file == ANNOUNCING_FILE.load(Ordering::Relaxed)
                    {
                        ANNOUNCING.store(false, Ordering::Relaxed);
                    }
                    i2s_tx = Some(tx);
                    wav_buffer = Some(buffer);
                    // The output stays up a while, so a press just after the
                    // story does not click; the idle loop powers it down.
                    quiet_since = Some(Instant::now());
                }
                PLAYING.store(false, Ordering::Relaxed);
            }

            _ => {}
        }
    }
}

/// Joins the network, gets an address, and serves network requests until
/// asked to stop, or, unless `stay_up`, until one fetch is done.
///
/// The whole connection lives inside one call because `acquire` lends the
/// session and the link from the radio: they cannot outlive it, and the link
/// must be polled the whole time. So being connected is time spent inside
/// this function, not a state it returns.
///
/// Returns why the connection failed, or `None` if it worked.
async fn bring_up(
    radio: &mut net::Radio<'_>,
    tls: Option<&tls::Client>,
    schedule: &mut teddiebox_wifikey::schedule::Schedule,
    stay_up: bool,
) -> Option<Unavailable> {
    let Some(config) = credentials() else {
        esp_println::println!(
            "teddiebox: net no credentials — type `net ssid <name>` then `net pw <passphrase>`"
        );
        // Not `Refused`, because nothing was tried. A box without credentials
        // already played `ConfigError` when reading the card failed.
        return Some(Unavailable::Unreachable);
    };

    let seed = net::seed();

    let stored = wifikey::key_for(&config.ssid, &config.password);
    let key = match &stored {
        Some(hex) => core::str::from_utf8(hex).expect("hex is ASCII"),
        None => config.password.as_str(),
    };

    let (mut session, mut link) = match radio.acquire(&config, key, seed) {
        Ok(pair) => pair,
        Err(e) => {
            esp_println::println!("teddiebox: net could not power the radio — {e:?}");
            return Some(Unavailable::Unreachable);
        }
    };
    esp_println::println!(
        "teddiebox: net associating with {}{}",
        config.ssid,
        if stored.is_some() {
            " using the stored key"
        } else {
            ""
        }
    );

    // Joining is mostly waiting for the access point, so the key's one-time
    // setup runs during it, after the join request is sent (hence the yield).
    // See `tls::Client::warm`.
    //
    // Not while audio plays: the setup blocks the executor for up to 3.5 s
    // (measured), which would interrupt playback. Then the first handshake
    // does it instead.
    let associate = async {
        let (connected, ()) = embassy_futures::join::join(session.connect(), async {
            embassy_futures::yield_now().await;
            match tls {
                Some(tls) if !PLAYING.load(Ordering::Relaxed) => tls.warm(),
                Some(_) => {
                    esp_println::println!("teddiebox: tls not warming the key during playback")
                }
                None => {}
            }
        })
        .await;
        connected
    };

    // The runner must be polled the whole time: it never returns, and nothing
    // else makes progress without it.
    match select(link.run(), associate).await {
        Either::First(_) => unreachable!("the runner never returns"),
        Either::Second(Err(e)) => {
            // The console prints the driver's error; the reducer only learns
            // whether the password was refused (check the card) or not (check
            // the router).
            let refused = net::refused_credentials(&e);
            // The passphrase was refused after the stored key was, so the
            // stored key is wrong too.
            if refused && stored.is_some() {
                wifikey::forget();
            }
            esp_println::println!(
                "teddiebox: net association refused — {e:?}{}",
                if refused { " — the passphrase" } else { "" }
            );
            net::release(session, link);
            return Some(if refused {
                Unavailable::Refused
            } else {
                Unavailable::Unreachable
            });
        }
        Either::Second(Ok(net::JoinedWith::GivenKey)) if stored.is_some() => {}
        // A join with the passphrase proves it worth storing a key for, and,
        // if a stored key was refused first, that the stored key is stale.
        Either::Second(Ok(_)) => {
            if stored.is_some() {
                wifikey::forget();
            }
            schedule.proved(config.ssid.as_bytes(), config.password.as_bytes());
        }
    }
    esp_println::println!("teddiebox: net associated — asking for an address");

    let stack = session.stack();
    // Joining and getting an address fail for different reasons; a box that
    // joins but gets no address must report that, not success.
    match select(
        link.run(),
        select(stack.wait_config_up(), Timer::after(DHCP_TIMEOUT)),
    )
    .await
    {
        Either::First(_) => unreachable!("the runner never returns"),
        Either::Second(Either::Second(())) => {
            esp_println::println!(
                "teddiebox: net associated but no DHCP lease in {} s",
                DHCP_TIMEOUT.as_secs()
            );
            net::release(session, link);
            // The passphrase was right; the network is not answering.
            return Some(Unavailable::Unreachable);
        }
        Either::Second(Either::First(())) => report_address(&stack),
    }

    // Connected. Stay here, polling the stack, until told to stop: dropping
    // the runner between requests would stall the connection.
    let until_down = async {
        loop {
            match NET_REQUEST.swap(REQUEST_NONE, Ordering::Relaxed) {
                NET_DOWN => break,
                NET_STATUS => report_address(&stack),
                // Runs here because it needs the stack, which only exists
                // between `acquire` and `release`. Awaited inline, so the
                // runner beside it keeps being polled.
                NET_GET => {
                    match tls {
                        None => {
                            esp_println::println!("teddiebox: tls context unavailable");
                            // Answer a waiting probe, or the figure on the
                            // plate would stay silent.
                            if let Some(FetchRequest {
                                ruid, probe: true, ..
                            }) = critical_section::with(|cs| *FETCH_REQUEST.borrow_ref(cs))
                            {
                                probe_ended(ruid);
                            }
                        }
                        Some(tls) => match critical_section::with(|cs| {
                            *FETCH_REQUEST.borrow_ref(cs)
                        }) {
                            None => esp_println::println!(
                                "teddiebox: get has no request queued — nothing to fetch"
                            ),
                            Some(FetchRequest {
                                ruid,
                                token,
                                probe: true,
                            }) => {
                                ask_length(tls, &stack, &config.server, ruid, token.as_ref()).await
                            }
                            Some(FetchRequest {
                                ruid,
                                token,
                                probe: false,
                            }) => {
                                fetch_story(tls, &stack, &config.server, ruid, token.as_ref()).await
                            }
                        },
                    }
                    // A connection made for one story ends with it, after the
                    // whole fetch is done.
                    if !stay_up {
                        break;
                    }
                }

                NET_PRIME => {
                    if let Some(tls) = tls {
                        let started = Instant::now();
                        match tls::prime(tls, &stack, &config.server).await {
                            Ok(()) => esp_println::println!(
                                "teddiebox: net primed in {} ms",
                                started.elapsed().as_millis()
                            ),
                            Err(e) => {
                                esp_println::println!("teddiebox: net could not prime — {e:?}")
                            }
                        }
                    }
                    // A figure placed while this ran uses this connection.
                    if !stay_up && NET_REQUEST.load(Ordering::Relaxed) != NET_GET {
                        break;
                    }
                }

                NET_TLS => match tls {
                    None => esp_println::println!(
                        "teddiebox: tls context unavailable — mbedtls would not start"
                    ),
                    Some(tls) => match tls::probe(tls, &stack, &config.server).await {
                        Ok(()) => esp_println::println!("teddiebox: tls ok"),
                        Err(e) => esp_println::println!("teddiebox: tls failed — {e:?}"),
                    },
                },
                _ => {}
            }
            Timer::after(Duration::from_millis(100)).await;
        }
    };
    match select(link.run(), until_down).await {
        Either::First(_) => unreachable!("the runner never returns"),
        Either::Second(()) => {}
    }

    net::release(session, link);
    esp_println::println!("teddiebox: net down");
    // The connection worked. Any fetch result has already been reported.
    None
}

/// Asks the server how long a figure's story is, and posts the answer.
///
/// Posts one whatever happens, because the reducer waits for it before
/// playing (see [`probe_ended`]).
async fn ask_length(
    tls: &tls::Client,
    stack: &embassy_net::Stack<'_>,
    server: &str,
    requested: u64,
    token: Option<&[u8; 32]>,
) {
    let wanted = tls::Wanted {
        server,
        ruid: requested.to_be_bytes(),
        token,
        from: None,
        etag: None,
    };
    let answer = tls::length(tls, stack, &wanted).await;
    match answer {
        Ok(teddiebox_cloud::Probed::Length(total)) => {
            esp_println::println!("teddiebox: ask {requested:016X} is {total} bytes");
            post_answer(requested, teddiebox_download::Answer::Length(total));
        }
        // Every other case plays what is on the card; only the console tells
        // them apart.
        Ok(other) => {
            esp_println::println!("teddiebox: ask {requested:016X} learned nothing — {other:?}");
            post_answer(requested, teddiebox_download::Answer::Nothing);
        }
        Err(e) => {
            esp_println::println!("teddiebox: ask {requested:016X} failed — {e:?}");
            post_answer(requested, teddiebox_download::Answer::Nothing);
        }
    }
}

/// Downloads a figure's story into [`DOWNLOAD_PIPE`], from wherever the
/// card's copy ends.
///
/// The media task writes the pipe to the card and reports a download that
/// arrives whole; this reports the endings only the net task sees.
async fn fetch_story(
    tls: &tls::Client,
    stack: &embassy_net::Stack<'_>,
    server: &str,
    requested: u64,
    token: Option<&[u8; 32]>,
) {
    // Recorded now, from this fetch's own request (see `FetchRequest`).
    FETCH_ACTIVE_RUID.store(requested, Ordering::Relaxed);
    let ruid = requested.to_be_bytes();
    // `content_path` reverses the UID itself, and the ruid is already
    // reversed, so undo that first.
    let mut uid = ruid;
    uid.reverse();
    let path = teddiebox_download::content_path(uid);

    DOWNLOAD_DIR.store(path.directory, Ordering::Relaxed);
    DOWNLOAD_FILE.store(path.file, Ordering::Relaxed);
    DOWNLOAD_SENT.store(0, Ordering::Relaxed);
    critical_section::with(|cs| *DOWNLOAD_PIPE.borrow_ref_mut(cs) = Pipe::new());
    // Only the card knows what is already downloaded, and only the media task
    // may read it, so the request waits for its answer. A resume asks only for
    // the rest, instead of downloading the whole file again.
    DOWNLOAD_FROM.store(0, Ordering::Relaxed);
    DOWNLOAD_AT.store(0, Ordering::Relaxed);

    // Asks the media task, and asks again if needed: it only runs
    // `service_download` between requests, not while a sound or story plays,
    // so a first ask made during a prompt goes unanswered.
    let mut handshake = Handshake::new();
    let mut step = handshake.requested(Instant::now().as_millis());
    let settled = loop {
        match step {
            Step::AskCard => DOWNLOAD_STATE.store(DOWNLOAD_PREPARING, Ordering::Relaxed),
            Step::Wait => {}
            settled => break settled,
        }
        Timer::after(Duration::from_millis(HANDSHAKE_POLL_MS)).await;
        // Check for an answer before the clock, so an answer that already
        // arrived is not replaced by a new ask.
        step = if DOWNLOAD_STATE.load(Ordering::Relaxed) == DOWNLOAD_PLANNED {
            handshake.card_answered(CardSays::from_offset(DOWNLOAD_FROM.load(Ordering::Relaxed)))
        } else {
            handshake.polled(Instant::now().as_millis())
        };
    };

    let from = match settled {
        Step::Play => {
            esp_println::println!("teddiebox: get the card already holds all of it");
            // Nothing to fetch: the story can play now.
            fetch_ended(FETCH_COMPLETED);
            DOWNLOAD_STATE.store(DOWNLOAD_IDLE, Ordering::Relaxed);
            return;
        }
        // **Not a fetch from zero**, which would truncate the partial file.
        // Stopping keeps what is cached for the next attempt.
        Step::GiveUp => {
            esp_println::println!(
                "teddiebox: get the card did not answer in {} s — \
                 leaving what is cached alone",
                teddiebox_download::DEADLINE_MS / 1_000
            );
            fetch_ended_if_silent(FETCH_UNREACHABLE);
            DOWNLOAD_STATE.store(DOWNLOAD_IDLE, Ordering::Relaxed);
            return;
        }
        Step::Fetch { from } => from,
        Step::Wait | Step::AskCard => {
            unreachable!("the loop above breaks on nothing else")
        }
    };

    if from > 0 {
        esp_println::println!("teddiebox: get resuming from {from}");
    }
    DOWNLOAD_ABORT.store(false, Ordering::Relaxed);
    if token.is_some() {
        esp_println::println!("teddiebox: get sending the tag's token");
    }
    let mut throttle = Throttle::new();
    let mut may_fetch = || {
        teddiebox_download::next_step(
            DOWNLOAD_ABORT.load(Ordering::Relaxed),
            throttle.update(
                PLAYING.load(Ordering::Relaxed),
                NOTHING_WAITING,
                RESUME_BELOW,
                PAUSE_ABOVE,
            ),
        )
    };
    let mut into_pipe = |bytes: &[u8]| -> usize {
        // Nobody is reading the pipe. Discard the rest so the download
        // ends instead of blocking for ever.
        if DOWNLOAD_ABORT.load(Ordering::Relaxed) {
            return bytes.len();
        }
        let taken = critical_section::with(|cs| DOWNLOAD_PIPE.borrow_ref_mut(cs).write(bytes));
        DOWNLOAD_SENT.fetch_add(taken as u32, Ordering::Relaxed);
        taken
    };
    let resume_etag = critical_section::with(|cs| DOWNLOAD_RESUME_ETAG.borrow_ref(cs).clone());
    let wanted = tls::Wanted {
        server,
        ruid,
        token,
        from: (from > 0).then_some(from),
        etag: resume_etag.as_ref(),
    };
    // Set when the response headers arrive, not before the request:
    // opening the file earlier could truncate a partial download for a
    // request that then fails.
    let mut on_head = |head: tls::Head<'_>| {
        DOWNLOAD_TOTAL.store(head.total.unwrap_or(0), Ordering::Relaxed);
        // Where the body actually starts, which may differ from what was
        // asked: zero if the server ignored the range.
        DOWNLOAD_AT.store(head.offset, Ordering::Relaxed);
        critical_section::with(|cs| {
            *DOWNLOAD_ETAG.borrow_ref_mut(cs) = head.etag.cloned();
        });
        DOWNLOAD_STATE.store(DOWNLOAD_RUNNING, Ordering::Relaxed);
    };
    let outcome = tls::fetch(
        tls,
        stack,
        &wanted,
        &mut on_head,
        &mut into_pipe,
        &mut may_fetch,
    )
    .await;
    // Ended either way, so the media task stops waiting for more bytes.
    DOWNLOAD_STATE.store(DOWNLOAD_ENDED, Ordering::Relaxed);

    match outcome {
        // Not reported as ready yet: the bytes are still in the pipe, and
        // the media task reports when they are on the card.
        Ok(got) => esp_println::println!(
            "teddiebox: get received {} bytes, crc32 {:08X}, {} s",
            got.bytes,
            got.crc32,
            got.seconds
        ),
        // Not a failure: the figure was lifted, and the reducer already
        // knows. Reporting "unreachable" would announce a network fault
        // for a story nobody wants.
        Err(tls::Error::Abandoned) => {
            esp_println::println!("teddiebox: get abandoned — nobody is waiting for it")
        }
        Err(e) => {
            // The console gets the exact error; the reducer only the reason
            // it acts on.
            esp_println::println!("teddiebox: get failed — {e:?}");
            fetch_ended(outcome_for(why_unavailable(&e)));
        }
    }
}

/// Prints the lease, or says there is not one.
fn report_address(stack: &embassy_net::Stack<'_>) {
    match stack.config_v4() {
        Some(config) => esp_println::println!(
            "teddiebox: net up — {} gateway {:?}",
            config.address,
            config.gateway
        ),
        None => esp_println::println!("teddiebox: net associated, no address yet"),
    }
}

/// Brings the radio up on demand and reports what it hears.
///
/// Also serves the console `net` commands. A scan needs no credentials, card
/// or network stack, so it tells "the radio does not work" apart from "the
/// password is wrong".
#[embassy_executor::task]
async fn net(
    wifi: esp_hal::peripherals::WIFI<'static>,
    sha: esp_hal::peripherals::SHA<'static>,
    rsa: esp_hal::peripherals::RSA<'static>,
    aes: esp_hal::peripherals::AES<'static>,
) {
    let mut radio = net::Radio::new(wifi);
    // Built once, before any connection. mbedtls keeps global state that can
    // only be set up once, so a failure lasts until reboot. The crypto
    // peripherals are passed in because mbedtls must be connected to them
    // before its first context is built; see `tls::init`.
    let tls = tls::init(sha, rsa, aes);
    if tls.is_none() {
        esp_println::println!("teddiebox: tls could not be initialised");
    }
    let mut schedule = teddiebox_wifikey::schedule::Schedule::new();

    loop {
        // Read the request once. Reading it twice would let the first read
        // swallow a request meant for the second.
        match NET_REQUEST.swap(REQUEST_NONE, Ordering::Relaxed) {
            NET_SCAN => {
                esp_println::println!("teddiebox: net scanning");
                // `AccessPointInfo` is `Clone` but not `Copy`, so the array
                // repeat syntax will not build it.
                let mut seen: [_; SCAN_LIMIT] = core::array::from_fn(|_| Default::default());

                match select(radio.scan(&mut seen), Timer::after(SCAN_TIMEOUT)).await {
                    Either::First(Ok(0)) => esp_println::println!(
                        "teddiebox: net heard nothing — radio came up, no access point in range"
                    ),
                    Either::First(Ok(found)) => {
                        for ap in &seen[..found] {
                            esp_println::println!(
                                "teddiebox: net   {} ch{} {} dBm {:?}",
                                ap.ssid.as_str(),
                                ap.channel,
                                ap.signal_strength,
                                ap.auth_method
                            );
                        }
                        esp_println::println!("teddiebox: net {found} access points");
                        // A full list means the driver stopped collecting; the
                        // highest channels were left out.
                        if found == SCAN_LIMIT {
                            esp_println::println!(
                                "teddiebox: net   list is truncated at {SCAN_LIMIT} — \
                             the high channels were not reached"
                            );
                        }
                    }
                    Either::First(Err(e)) => {
                        esp_println::println!("teddiebox: net scan failed — {e:?}")
                    }
                    // No answer at all. The driver sends `ScanDone` even when it
                    // finds nothing, so this means the radio never started.
                    Either::Second(()) => esp_println::println!(
                        "teddiebox: net scan timed out after {} s — the radio never answered",
                        SCAN_TIMEOUT.as_secs()
                    ),
                }
            }
            // From the console: any failure is printed inside, and no figure
            // is waiting for the result.
            NET_UP => {
                let _ = bring_up(&mut radio, tls.as_ref(), &mut schedule, true).await;
            }
            NET_PRIME => {
                esp_println::println!("teddiebox: net coming up to prime the first ask");
                // Put back for `bring_up`'s loop to serve, as with a fetch.
                NET_REQUEST.store(NET_PRIME, Ordering::Relaxed);
                if bring_up(&mut radio, tls.as_ref(), &mut schedule, false)
                    .await
                    .is_some()
                {
                    // It never reached the loop that serves the request. Only
                    // the prime is dropped; a figure's request that replaced
                    // it in the meantime stays queued.
                    let _ = NET_REQUEST.compare_exchange(
                        NET_PRIME,
                        REQUEST_NONE,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    );
                }
            }
            // These are served inside `bring_up` while connected; reaching
            // them here means the network is down.
            NET_STATUS | NET_DOWN | NET_TLS => {
                esp_println::println!("teddiebox: net is down")
            }
            // A fetch or probe with the radio off. The radio is switched on
            // for it and off again afterwards: the plate is read less reliably
            // while the radio runs, and there is no other reason to stay
            // connected.
            NET_GET => match critical_section::with(|cs| *FETCH_REQUEST.borrow_ref(cs)) {
                None => {
                    esp_println::println!("teddiebox: get has no request queued — nothing to fetch")
                }
                Some(FetchRequest { ruid, probe, .. }) => {
                    // Recorded before anything can end the fetch.
                    FETCH_ACTIVE_RUID.store(ruid, Ordering::Relaxed);
                    esp_println::println!(
                        "teddiebox: net coming up {}",
                        if probe {
                            "to ask about a figure"
                        } else {
                            "for a story"
                        }
                    );
                    // Put back for `bring_up`'s loop to serve; the read at the
                    // top of this loop took it out.
                    NET_REQUEST.store(NET_GET, Ordering::Relaxed);
                    let gave_up = bring_up(&mut radio, tls.as_ref(), &mut schedule, false).await;
                    // If `bring_up` failed before its loop, the request is
                    // still queued. Clear it, or the same fetch would be
                    // retried for ever, and the retries interrupt playback (up
                    // to 291 audio DMA restarts in 263 s, measured).
                    if gave_up.is_some() {
                        NET_REQUEST.store(REQUEST_NONE, Ordering::Relaxed);
                    }
                    // Answer a probe that is still unanswered: the reducer
                    // keeps the figure silent until it hears back, and a
                    // network failure is no reason not to play the card's copy.
                    if probe {
                        if !answer_waiting() {
                            esp_println::println!(
                                "teddiebox: net would not come up to ask — playing what is here"
                            );
                            probe_ended(ruid);
                        }
                    }
                    // If `bring_up` could not connect (no credentials, join
                    // failed, no address), nobody else reports it, so report
                    // it here, with the right reason: a wrong passphrase
                    // should not be announced as a network fault.
                    else if DOWNLOAD_STATE.load(Ordering::Relaxed) == DOWNLOAD_IDLE
                        && fetch_ended_if_silent(outcome_for(
                            gave_up.unwrap_or(Unavailable::Unreachable),
                        ))
                    {
                        esp_println::println!("teddiebox: net would not come up for it");
                    }
                }
            },
            // Idle with the radio off: the only time deriving the Wi-Fi key or
            // writing it to flash does not delay a connection.
            _ => wifikey::pass(&mut schedule, credentials, PLAYING.load(Ordering::Relaxed)),
        }
        Timer::after(Duration::from_millis(100)).await;
    }
}

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    let p = esp_hal::init(esp_hal::Config::default());

    // Clear the ROM's force-download-boot request. It survives a reset (that
    // is how `dl` works), so leaving it set would send every later reset into
    // download mode.
    //
    // Before the OTA check below, which can reboot to revert: with this bit
    // still set, that reboot would go into download mode instead.
    esp_hal::peripherals::LPWR::regs()
        .option1()
        .modify(|_, w| w.force_download_boot().clear_bit());

    // Before anything else, including the stack paint below: this catches an
    // image that crashed on its *previous* boot before reaching `mark_valid`,
    // and nothing above this line can have been the cause.
    ota::confirm_boot_or_revert();

    // Paint the stack before anything uses it deeply, so the `stack` command
    // can measure how much was used.
    //
    // Surrounded by prints, because a fault in the paint leaves the box
    // silent, with no panic message; the prints show whether it got there.
    esp_println::println!("teddiebox: painting the stack");
    stack::paint();
    esp_println::println!("teddiebox: stack painted");

    // Only the radio stack (esp-radio and TLS) allocates; libopus must not
    // (it panics instead of allocating).
    //
    // The heap is a static, so it comes out of the stack: `esp-hal` places
    // the stack between the end of `.bss` and the top of DRAM. Check the
    // stack size, not the heap size, to know whether the box still boots.
    esp_alloc::heap_allocator!(size: RADIO_HEAP);

    let timg0 = TimerGroup::new(p.TIMG0);
    esp_rtos::start(timg0.timer0, p.FROM_CPU_INTR0);

    let mut board = BoardPins::new(p.GPIO45, p.GPIO47);
    let mut gates = Gates::at_reset();

    // Deep sleep consumes this peripheral, so it is held in an `Option` until
    // then, like other single-use peripherals.
    let mut lpwr = Some(p.LPWR);

    // The LED is on this rail, so switch it on before anything, including the
    // setup portal below, uses the LED.
    board.apply(gates.power(Rail::Peripherals, true));

    // Set up here, before the setup branch below, because setup mode also
    // uses the LED. The channels borrow the timer; `main` never returns, so
    // it lives long enough.
    let ledc = led::controller(p.LEDC);
    let timer = led::timer(&ledc);
    let rgb = match timer.as_ref() {
        Ok(timer) => match led::Rgb::new(&ledc, timer, p.GPIO19, p.GPIO18, p.GPIO17) {
            Ok(rgb) => Some(rgb),
            Err(()) => {
                esp_println::println!("teddiebox: LED channels would not configure");
                None
            }
        },
        Err(()) => {
            esp_println::println!("teddiebox: LED timer would not configure");
            None
        }
    };

    // The ears are active low, so they use a pull-up: an unconnected input
    // reads as "not pressed" instead of floating.
    let up = InputConfig::default().with_pull(Pull::Up);
    let larger = Input::new(p.GPIO20, up);
    let smaller = Input::new(p.GPIO21, up);

    // Started before the setup branch, because setup mode needs battery
    // monitoring too: its access point runs the radio the whole time. Other
    // tasks are started after the branch.
    let mut adc_config = AdcConfig::new();
    let battery = adc_config.enable_pin(p.GPIO9, Attenuation::_11dB);
    let charger = adc_config.enable_pin(p.GPIO8, Attenuation::_11dB);
    spawner.spawn(battery::sense(Adc::new(p.ADC1, adc_config), battery, charger).unwrap());

    // Both ears held at power-on start the setup portal. Checked before
    // `net` is started (the only other user of `p.WIFI`) and before the rest
    // of the boot, because setup mode runs almost none of the tasks below:
    // no decoder, codec, NFC or media loop.
    //
    // Skipping those tasks does not make room for the portal: its buffers
    // are held across `await` points, so they are part of this task's future
    // in `.bss`, and the stack gets whatever `.bss` leaves. See the
    // measurements in `portal.rs`.
    //
    // Active low, so held reads low. In setup mode the ears are not read
    // again.
    if larger.is_low() && smaller.is_low() {
        esp_println::println!("teddiebox: both ears held — setup portal");

        // Nothing drains `INPUT_EVENTS` here: the reducer lives in the media
        // task and that is never spawned. Told now rather than discovered
        // later, so `sense` keeps its readings to the console instead of
        // filling an eight-slot queue and then complaining about it once
        // every two seconds for as long as the portal is up.
        battery::SETUP_MODE.store(true, Ordering::Relaxed);

        // Nothing before this point brings the card up: the rest of the boot
        // does that lazily, inside `media`, on the first command that needs
        // it. Setup mode never reaches `media`, so the same bus and the same
        // rail are brought up here instead, once, so `portal::run` has the
        // card it needs to read and rewrite `CONFIG.TXT`.
        board.apply(gates.power(Rail::Storage, true));
        Timer::after(Duration::from_millis(50)).await;

        // In other modes the main loop below shows `LED_REQUEST`. This branch
        // never reaches that loop, so it sets the LED directly.
        let paint = |state: LedState| {
            if let Some(rgb) = rgb.as_ref() {
                if let Ok(lit) = gates.led(colour_for(state)) {
                    rgb.apply(&lit, board::LED_DUTY);
                }
            }
        };

        // Taken before the card is mounted, because the failure path below
        // parks, which is an `await`: anything alive across it becomes part
        // of this task's future, in `.bss`. Doing this first keeps the
        // 812-byte `Mounted` out of the future.
        let Some(scratch) = audio::take_scratch_bytes() else {
            esp_println::println!(
                "teddiebox: portal cannot have the decode scratch — it is in use"
            );
            paint(LedState::Error);
            portal::park().await
        };

        // The access point starts even without a card: the page then says
        // what is wrong, and only saving is refused. The console prints the
        // exact reason.
        let card = match Spi::new(p.SPI2, storage::init_config()) {
            Ok(spi) => {
                let spi = spi
                    .with_sck(p.GPIO35)
                    .with_mosi(p.GPIO38)
                    .with_miso(p.GPIO36);
                let cs = Output::new(p.GPIO34, Level::High, OutputConfig::default());
                match storage::Mounted::open(spi, cs, esp_hal::delay::Delay::new()) {
                    Ok(card) => Some(card),
                    Err(reason) => {
                        esp_println::println!(
                            "teddiebox: portal could not mount the card — {reason}"
                        );
                        None
                    }
                }
            }
            Err(_) => {
                esp_println::println!("teddiebox: portal's SPI would not configure");
                None
            }
        };

        let seed = net::seed();

        // Setup mode reuses the decoder's scratch memory. `portal::run`'s
        // future (socket buffers, the card and the radio's `StackResources`)
        // is moved into the 51,712 bytes of `audio::SCRATCH`, which the
        // decoder does not use in this mode, so only a pointer stays in this
        // task's future. That keeps the stack as large as without the portal;
        // `portal::place` has the numbers.
        //
        // The scratch is taken through the same check the decoder uses, so
        // both can never have it at once.
        let Some(portal) = portal::place(
            scratch,
            portal::run(p.WIFI, p.UART0, p.GPIO44, card, seed, paint),
        ) else {
            // `place` has already printed what did not fit. Park rather than
            // reset: the ears may still be held, so a reset would come
            // straight back here.
            portal::park().await
        };
        portal.await
    }

    spawner.spawn(net(p.WIFI, p.SHA, p.RSA, p.AES).unwrap());

    // One task per ear, each waiting on its pin's edge: the GPIO hardware
    // latches a press until it is read, which a 2 ms poll could not
    // guarantee while the decoder uses 69% of the CPU.
    spawner.spawn(inputs::ear(Ear::Larger, "larger ear", larger).unwrap());
    spawner.spawn(inputs::ear(Ear::Smaller, "smaller ear", smaller).unwrap());
    spawner.spawn(inputs::inputs(Input::new(p.GPIO7, up)).unwrap());

    // Let the console output finish before UART0 is reconfigured below.
    // Otherwise the last boot lines still in the transmit FIFO arrive cut off
    // and mixed into the next line.
    drain_console();

    // UART0's receive half. esp-println keeps the transmit half.
    let mut console = UartRx::new(p.UART0, UartConfig::default().with_baudrate(115200))
        .expect("UART0 receive")
        .with_rx(p.GPIO44);
    let mut watch = CommandWatch::new();
    // Cleared when the jingle is requested, so it only plays once.
    let mut startup_pending = true;
    // Separate from `startup_pending`: the jingle only needs the codec, but
    // confirming an update also needs the card, which is mounted later.
    let mut boot_confirmed = false;
    esp_println::println!(
        "teddiebox: dl rb | t wav taf play <id>[/<id>|<16hex>] stop (loud) | sd | nfc pw slix slixp lock mem <2hex> <2hex> token | net scan ssid <name> pw <pass> up down tls status | get <16hex> | crc <16hex> | stack | cinit cdown cset cclr out spk | pcm <2hex> | batlog <seconds> | slap <2hex> slapt <2hex> | plate on|off | pcmcrc on|off | awake on|off | sleep | autosleep on|off | reval"
    );

    // Here rather than next to `ota::confirm_boot_or_revert()`: loaded before
    // `stack::paint()`, its console output was overwritten by the paint and
    // came out garbled. `flash::flash()` lends one handle at a time; each call
    // takes it and gives it back.
    //
    // Setup mode never reaches this line, so it never loads the identity.
    // That does not matter now, because the portal makes no outgoing TLS
    // connection.
    identity::load();
    wifikey::load();

    // Audio out on I2S: DIN 10, BCLK 11, WCLK 12, at the rate the codec's PLL
    // was configured for. The SD card is SPI2 on CLK 35, MOSI 38, MISO 36 with
    // CS 34, created at the specification's 400 kHz initialisation rate;
    // storage.rs raises it once the card has identified itself.
    //
    // Both go to the media task, which serves everything that needs them.
    match (
        I2s::new(
            p.I2S0,
            p.DMA_CH0,
            TdmConfig::new_tdm_philips()
                .with_sample_rate(Rate::from_hz(tone::SAMPLE_RATE_HZ))
                .with_data_format(DataFormat::Data16Channel16)
                .with_channels(Channels::STEREO),
        ),
        Spi::new(p.SPI2, storage::init_config()),
    ) {
        (Ok(i2s), Ok(spi)) => {
            let i2s_tx = i2s
                .i2s_tx
                .with_bclk(p.GPIO11)
                .with_ws(p.GPIO12)
                .with_dout(p.GPIO10)
                .build();
            let spi = spi
                .with_sck(p.GPIO35)
                .with_mosi(p.GPIO38)
                .with_miso(p.GPIO36);
            // Starts high (not selected): a card is selected when this line
            // goes low.
            let cs = Output::new(p.GPIO34, Level::High, OutputConfig::default());

            let tone_buffer = esp_hal::dma_loop_buffer!(tone::SINE.len() * 4);
            // About 170 ms of audio at 48 kHz stereo 16-bit. Its low-water
            // mark is measured to check the size.
            let wav_buffer = esp_hal::dma_tx_stream_buffer!(audio::BUFFER_BYTES);

            spawner.spawn(media(spi, cs, i2s_tx, tone_buffer, wav_buffer).unwrap());

            // The reader is on its own bus: SCLK 4, MOSI 2, MISO 3, CS 1, with
            // IRQ on 13. It shares only the power rail with the card.
            match Spi::new(p.SPI3, nfc::bus_config()) {
                Ok(nfc_spi) => {
                    let nfc_spi = nfc_spi
                        .with_sck(p.GPIO4)
                        .with_mosi(p.GPIO2)
                        .with_miso(p.GPIO3);
                    let nfc_cs = Output::new(p.GPIO1, Level::High, OutputConfig::default());
                    let nfc_irq = Input::new(p.GPIO13, InputConfig::default());
                    // The storage rail (gate 47) also powers the reader, so
                    // switch it on here when polling is on from boot, as
                    // `plate on` does. Before the spawn, because the task
                    // waits only 50 ms before talking to the reader.
                    if nfc::PLATE_POLLING.load(Ordering::Relaxed) {
                        board.apply(gates.power(Rail::Storage, true));
                    }
                    spawner.spawn(nfc::nfc_reader(nfc_spi, nfc_cs, nfc_irq).unwrap());
                }
                Err(_) => esp_println::println!("teddiebox: NFC SPI would not configure"),
            }
        }
        (Err(_), _) => esp_println::println!("teddiebox: I2S would not configure"),
        (_, Err(_)) => esp_println::println!("teddiebox: SPI would not configure"),
    }

    // Devices need a moment after their rail comes up. Without this the
    // accelerometer misses the scan but answers a few milliseconds later.
    Timer::after(Duration::from_millis(50)).await;

    // The codec and the accelerometer are on the rail switched on above, so
    // scan the bus only now.
    match I2c::new(p.I2C0, I2cConfig::default()) {
        Ok(i2c) => {
            let mut i2c = i2c.with_sda(p.GPIO5).with_scl(p.GPIO6);
            i2c_bus::scan_i2c(&mut i2c);
            let reset = Output::new(p.GPIO26, Level::Low, OutputConfig::default());
            spawner.spawn(i2c_bus::i2c_bus(i2c, reset).unwrap());
        }
        Err(_) => esp_println::println!("teddiebox: I2C would not configure"),
    }

    // The LEDC peripheral holds the colour, so this loop only has to update
    // it often enough that a change shows promptly.
    const LED_STEP: Duration = Duration::from_millis(20);

    // What the LED shows now, so a pass without a new request keeps the same
    // colour.
    let mut shown = LedState::Booting;

    // On a wake with an empty pack, show red and go back to sleep before the
    // codec, card, radio or jingle start. Speaking would draw near-full power
    // for seconds on every ear press, draining the unprotected cells below
    // the cutoff; red costs only milliamps.
    //
    // Only on a wake from deep sleep. A cold boot on a flat pack still starts
    // and announces the problem.
    if reset_reason(Cpu::ProCpu) == Some(SocResetReason::CoreDeepSleep) {
        if let Some(mv) = battery::pack_says_empty().await {
            esp_println::println!("teddiebox: pack {mv} mV on wake — below the cutoff, going back");
            if let Some(rgb) = rgb.as_ref() {
                if let Ok(red) = gates.led(colour_for(LedState::BatteryCritical)) {
                    rgb.apply(&red, board::LED_DUTY);
                }
            }
            Timer::after(Duration::from_secs(2)).await;
            go_dark(&mut board, &mut gates, rgb.as_ref()).await;
            if let Err(reason) = sleep_now(&mut lpwr).await {
                esp_println::println!("teddiebox: sleep not armed — {reason}, carrying on awake");
            }
        }
    }

    loop {
        if let Some(rgb) = rgb.as_ref() {
            // An unknown value can only be a bug; keep the last colour rather
            // than going dark.
            if let Some(state) = LedState::from_code(LED_REQUEST.load(Ordering::Relaxed)) {
                shown = state;
            }
            // Leave the LED dark rather than panic if the rail is ever off
            // here (no current path does that).
            if let Ok(lit) = gates.led(colour_for(shown)) {
                rgb.apply(&lit, board::LED_DUTY);
            }
        }

        // The start-up jingle, once the codec is ready.
        //
        // It also hides the click of powering the class-D amplifier, as the
        // original firmware does.
        if startup_pending && i2c_bus::CODEC_READY.load(Ordering::Relaxed) {
            startup_pending = false;
            SOUND_REQUEST.store(Sound::Startup.file(), Ordering::Relaxed);
            STARTUP_SOUNDED.store(true, Ordering::Relaxed);
        }

        if !boot_confirmed
            && i2c_bus::CODEC_READY.load(Ordering::Relaxed)
            && CARD_MOUNTED.load(Ordering::Relaxed)
        {
            boot_confirmed = true;
            ota::mark_valid();
        }

        // A sound the box wants to play, such as a warning. Started here,
        // because this loop controls the rails.
        let pending = SOUND_REQUEST.swap(NO_SOUND, Ordering::Relaxed);
        if pending != NO_SOUND {
            board.apply(gates.power(Rail::Storage, true));
            i2c_bus::request_output(i2c_bus::OUTPUT_UP);
            CONTENT_DIRECTORY.store(LANGUAGE.content_directory(), Ordering::Relaxed);
            CONTENT_FILE.store(pending, Ordering::Relaxed);
            // Set here rather than by the media task, so there is no moment
            // when the announcement is queued but not marked. The file is
            // recorded so only its own playback clears the flag.
            ANNOUNCING_DIRECTORY.store(LANGUAGE.content_directory(), Ordering::Relaxed);
            ANNOUNCING_FILE.store(pending, Ordering::Relaxed);
            ANNOUNCING.store(true, Ordering::Relaxed);
            REQUEST.store(REQUEST_CONTENT, Ordering::Relaxed);
        }

        // The box said it is turning off. Do it once the announcement has
        // finished.
        if SHUTTING_DOWN.load(Ordering::Relaxed) && !ANNOUNCING.load(Ordering::Relaxed) {
            // Last chance to save the position. Ask the media task and wait,
            // but only briefly: losing the position is better than a box that
            // will not turn off.
            FLUSH_PLACE.store(true, Ordering::Relaxed);
            for _ in 0..20 {
                if !FLUSH_PLACE.load(Ordering::Relaxed) {
                    break;
                }
                Timer::after(Duration::from_millis(25)).await;
            }
            // The reason was already printed by `perform`.
            esp_println::println!("teddiebox: going dark");
            go_dark(&mut board, &mut gates, rgb.as_ref()).await;
            // Deep sleep, because a park keeps the CPU and PLLs running (tens
            // of milliamps) and would keep draining the pack. `autosleep off`
            // parks instead.
            if AUTO_SLEEP.load(Ordering::Relaxed) {
                if let Err(reason) = sleep_now(&mut lpwr).await {
                    esp_println::println!("teddiebox: sleep not armed — {reason}, parking instead");
                }
            }
            // Sound and lights are off; now stop every task. Reached for a
            // park, and when sleep could not be armed: a box that drains can
            // be recharged, but one asleep with no way to wake is stuck.
            PARKED.store(true, Ordering::Relaxed);
            park_task().await;
        }

        let mut buf = [0u8; 16];
        if let Ok(n) = console.read_buffered(&mut buf) {
            let command = buf[..n].iter().find_map(|&b| watch.feed(b));

            // A release image only accepts `dl`. One check around the whole
            // dispatch, not one per command, so a new command is covered
            // automatically, and the optimiser drops all of it (see `BENCH`).
            if !BENCH {
                match command {
                    Some(Command::DownloadMode) => {
                        i2c_bus::quieten_codec().await;
                        reboot_to_download(&mut board, &mut gates)
                    }
                    Some(_) => not_in_this_build(),
                    None => {}
                }
                Timer::after(LED_STEP).await;
                continue;
            }

            match command {
                Some(Command::DownloadMode) => {
                    i2c_bus::quieten_codec().await;
                    reboot_to_download(&mut board, &mut gates)
                }
                Some(Command::Reboot) => {
                    i2c_bus::quieten_codec().await;
                    reboot(&mut board, &mut gates)
                }
                Some(Command::Tone) => {
                    // The output is off until something plays, so every audio
                    // command must switch it on first.
                    i2c_bus::request_output(i2c_bus::OUTPUT_UP);
                    REQUEST.store(REQUEST_TONE, Ordering::Relaxed);
                }
                Some(Command::PlayWav) => {
                    board.apply(gates.power(Rail::Storage, true));
                    i2c_bus::request_output(i2c_bus::OUTPUT_UP);
                    REQUEST.store(REQUEST_WAV, Ordering::Relaxed);
                }
                Some(Command::Nfc) => {
                    board.apply(gates.power(Rail::Storage, true));
                    nfc::NFC_REQUEST.store(nfc::NFC_INVENTORY, Ordering::Relaxed);
                }
                Some(Command::Unlock) => {
                    board.apply(gates.power(Rail::Storage, true));
                    nfc::NFC_REQUEST.store(nfc::NFC_UNLOCK, Ordering::Relaxed);
                }
                Some(Command::ForceUnlock) => {
                    board.apply(gates.power(Rail::Storage, true));
                    nfc::NFC_REQUEST.store(nfc::NFC_FORCE_UNLOCK, Ordering::Relaxed);
                }
                Some(Command::CodecSet {
                    page,
                    register,
                    value,
                }) => {
                    if i2c_bus::codec_override_set(page, register, value) {
                        esp_println::println!(
                            "teddiebox: codec page {page} register {register:#04x} -> {value:#04x} on next cinit"
                        );
                    } else {
                        esp_println::println!("teddiebox: no override slots left — cclr first");
                    }
                }
                Some(Command::CodecClear) => {
                    i2c_bus::codec_overrides_clear();
                    esp_println::println!("teddiebox: codec overrides cleared");
                }
                Some(Command::CodecInit) => {
                    i2c_bus::CODEC_REINIT.store(true, Ordering::Relaxed);
                }
                Some(Command::CodecDown) => {
                    i2c_bus::CODEC_POWER_DOWN.store(true, Ordering::Relaxed);
                }
                Some(Command::Output(on)) => {
                    i2c_bus::request_output(if on {
                        i2c_bus::OUTPUT_UP
                    } else {
                        i2c_bus::OUTPUT_DOWN
                    });
                }
                Some(Command::Speaker(on)) => {
                    i2c_bus::SPEAKER_REQUEST.store(
                        if on {
                            i2c_bus::SPEAKER_UNMUTE
                        } else {
                            i2c_bus::SPEAKER_MUTE
                        },
                        Ordering::Relaxed,
                    );
                }
                Some(Command::HeadphoneStatus) => {
                    i2c_bus::HEADPHONE_REPORT.store(true, Ordering::Relaxed);
                }
                // Sets both the static the codec bring-up reads and the event
                // for the reducer, which switches the output and its volume
                // scale together.
                Some(Command::Headphones(on)) => {
                    i2c_bus::HEADPHONES_IN.store(on, Ordering::Relaxed);
                    esp_println::println!(
                        "teddiebox: headphones forced {}",
                        if on { "in" } else { "out" }
                    );
                    if inputs::INPUT_EVENTS
                        .try_send(Event::Headphones(on))
                        .is_err()
                    {
                        esp_println::println!("teddiebox: input queue full, jack change dropped");
                    }
                }
                Some(Command::NetScan) => {
                    NET_REQUEST.store(NET_SCAN, Ordering::Relaxed);
                }
                Some(Command::NetSsid(value)) => {
                    esp_println::println!("teddiebox: net ssid set to {value}");
                    set_ssid(value);
                }
                // Not echoed, like `pw`.
                Some(Command::NetPassword(value)) => {
                    esp_println::println!("teddiebox: net passphrase set ({} chars)", value.len());
                    set_password(value);
                }
                Some(Command::NetUp) => {
                    NET_REQUEST.store(NET_UP, Ordering::Relaxed);
                }
                Some(Command::NetDown) => {
                    NET_REQUEST.store(NET_DOWN, Ordering::Relaxed);
                }
                Some(Command::Get(ruid)) => {
                    // One critical section: the token last read by the
                    // console `token` command goes into this request.
                    critical_section::with(|cs| {
                        let token = *nfc::TAG_TOKEN.borrow_ref(cs);
                        *FETCH_REQUEST.borrow_ref_mut(cs) = Some(FetchRequest {
                            ruid: u64::from_be_bytes(ruid),
                            token,
                            probe: false,
                        });
                    });
                    NET_REQUEST.store(NET_GET, Ordering::Relaxed);
                }
                Some(Command::StackReport) => stack::report(),
                Some(Command::OtaStatus) => ota::status(),
                Some(Command::OtaWriteProbe) => ota::write_probe(),
                Some(Command::OtaBoot { slot }) => ota::arm_boot(slot),
                Some(Command::ReadToken) => {
                    board.apply(gates.power(Rail::Storage, true));
                    nfc::NFC_REQUEST.store(nfc::NFC_READ_TOKEN, Ordering::Relaxed);
                }
                Some(Command::PlayCache { directory, file }) => {
                    board.apply(gates.power(Rail::Storage, true));
                    CONTENT_DIRECTORY.store(directory, Ordering::Relaxed);
                    CONTENT_FILE.store(file, Ordering::Relaxed);
                    REQUEST.store(REQUEST_CACHE, Ordering::Relaxed);
                }
                Some(Command::Crc { directory, file }) => {
                    board.apply(gates.power(Rail::Storage, true));
                    CONTENT_DIRECTORY.store(directory, Ordering::Relaxed);
                    CONTENT_FILE.store(file, Ordering::Relaxed);
                    REQUEST.store(REQUEST_CRC, Ordering::Relaxed);
                }
                Some(Command::NetTls) => {
                    NET_REQUEST.store(NET_TLS, Ordering::Relaxed);
                }
                Some(Command::NetStatus) => {
                    NET_REQUEST.store(NET_STATUS, Ordering::Relaxed);
                }
                Some(Command::ReadMemory { first, count }) => {
                    board.apply(gates.power(Rail::Storage, true));
                    nfc::NFC_MEM_RANGE.store(
                        (u32::from(first) << 8) | u32::from(count),
                        Ordering::Relaxed,
                    );
                    nfc::NFC_REQUEST.store(nfc::NFC_READ_MEMORY, Ordering::Relaxed);
                }
                Some(Command::Lock) => {
                    board.apply(gates.power(Rail::Storage, true));
                    nfc::NFC_REQUEST.store(nfc::NFC_LOCK, Ordering::Relaxed);
                }
                // Sleep now, whatever `autosleep` says.
                Some(Command::Sleep) => {
                    go_dark(&mut board, &mut gates, rgb.as_ref()).await;
                    if let Err(reason) = sleep_now(&mut lpwr).await {
                        esp_println::println!(
                            "teddiebox: sleep not armed — {reason}; the box is awake and dark, \
                             and the ears are gone until rb"
                        );
                    }
                }
                Some(Command::StayAwake(on)) => {
                    STAY_AWAKE.store(on, Ordering::Relaxed);
                    esp_println::println!(
                        "teddiebox: staying awake {}",
                        if on { "on" } else { "off" }
                    );
                }
                Some(Command::Revalidate) => {
                    critical_section::with(|cs| ASKED.borrow_ref_mut(cs).forget_all());
                    esp_println::println!(
                        "teddiebox: every figure will be asked about again on its next placement"
                    );
                }
                Some(Command::AutoSleep(on)) => {
                    AUTO_SLEEP.store(on, Ordering::Relaxed);
                    esp_println::println!(
                        "teddiebox: the session ends in {}",
                        if on { "deep sleep" } else { "a park" }
                    );
                }
                Some(Command::EarsSkip(on)) => {
                    set_ears_skip(on);
                    esp_println::println!(
                        "teddiebox: a held ear {}",
                        if on {
                            "skips a chapter"
                        } else {
                            "only changes the volume"
                        }
                    );
                }
                Some(Command::PcmCrc(on)) => {
                    audio::PCM_CRC.store(on, Ordering::Relaxed);
                    esp_println::println!(
                        "teddiebox: playback checksum {} from the next story",
                        if on { "on" } else { "off" }
                    );
                }

                Some(Command::Plate(on)) => {
                    if !on {
                        nfc::PLATE_REPORT.store(true, Ordering::Relaxed);
                    }
                    if on {
                        board.apply(gates.power(Rail::Storage, true));
                    }
                    nfc::PLATE_POLLING.store(on, Ordering::Relaxed);
                    esp_println::println!(
                        "teddiebox: plate polling {}",
                        if on { "on" } else { "off" }
                    );
                }
                Some(Command::Password(value)) => {
                    nfc::NFC_PASSWORD.store(value, Ordering::Relaxed);
                    // Not echoed: it is a credential, and console captures
                    // are often saved to files.
                    esp_println::println!("teddiebox: nfc password set");
                }
                Some(Command::PlayTaf) => {
                    board.apply(gates.power(Rail::Storage, true));
                    i2c_bus::request_output(i2c_bus::OUTPUT_UP);
                    REQUEST.store(REQUEST_TAF, Ordering::Relaxed);
                }
                Some(Command::PlaySound { file }) => {
                    board.apply(gates.power(Rail::Storage, true));
                    i2c_bus::request_output(i2c_bus::OUTPUT_UP);
                    CONTENT_DIRECTORY.store(LANGUAGE.content_directory(), Ordering::Relaxed);
                    CONTENT_FILE.store(file, Ordering::Relaxed);
                    REQUEST.store(REQUEST_CONTENT, Ordering::Relaxed);
                }
                Some(Command::PlayContent { directory, file }) => {
                    board.apply(gates.power(Rail::Storage, true));
                    i2c_bus::request_output(i2c_bus::OUTPUT_UP);
                    CONTENT_DIRECTORY.store(directory, Ordering::Relaxed);
                    CONTENT_FILE.store(file, Ordering::Relaxed);
                    REQUEST.store(REQUEST_CONTENT, Ordering::Relaxed);
                }
                Some(Command::DumpPcm { frames }) => {
                    board.apply(gates.power(Rail::Storage, true));
                    PCM_FRAMES.store(frames, Ordering::Relaxed);
                    REQUEST.store(REQUEST_PCM, Ordering::Relaxed);
                }
                Some(Command::BatteryLog { seconds }) => {
                    battery::BATLOG_EVERY.store(seconds, Ordering::Relaxed);
                    if seconds == 0 {
                        esp_println::println!("teddiebox: batlog off");
                    } else {
                        esp_println::println!("teddiebox: batlog every {seconds} s");
                        esp_println::println!("batlog,ms,raw,mv,playing,charger_raw");
                    }
                }
                Some(Command::SlapTimeLimit { limit }) => {
                    i2c_bus::SLAP_TIME_LIMIT.store(limit, Ordering::Relaxed);
                }
                Some(Command::SlapThreshold { threshold }) => {
                    i2c_bus::SLAP_THRESHOLD.store(threshold, Ordering::Relaxed);
                }
                Some(Command::Stop) => {
                    audio::STOP.store(true, Ordering::Relaxed);
                }
                // Only setup mode's console handles this; outside setup mode
                // the access point it configures is not running.
                Some(Command::SetupPassword(_)) => esp_println::println!(
                    "teddiebox: setup pw only works in setup mode — hold both ears at switch-on"
                ),
                Some(Command::Storage) => {
                    // Switched on here, because this loop owns the pins, and
                    // left on afterwards so the card never loses power mid-read.
                    board.apply(gates.power(Rail::Storage, true));
                    REQUEST.store(REQUEST_WALK, Ordering::Relaxed);
                }
                None => {}
            }
        }

        Timer::after(LED_STEP).await;
    }
}
