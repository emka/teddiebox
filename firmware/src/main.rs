#![no_std]
#![no_main]

mod audio;
mod index;
mod led;
mod libc_shim;
mod net;
mod nfc;
mod pins;
mod stack;
mod storage;
mod tls;

use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};

use critical_section::Mutex as CsMutex;
use embassy_executor::Spawner;
use embassy_futures::select::{select, Either};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use heapless::String;
use teddiebox_config::{Config, Overridden};
use teddiebox_core::console::{MAX_PASSPHRASE, MAX_SSID};

use esp_backtrace as _;
use esp_hal::analog::adc::{Adc, AdcConfig, Attenuation};
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::i2s::master::{Channels, DataFormat, I2s, TdmConfig};
use esp_hal::spi::master::Spi;
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_hal::uart::{Config as UartConfig, UartRx};
use lis3dh::{regs as lis, Lis3dh};
use teddiebox_core::board::{self, Colour, Gates, Rail};
use teddiebox_core::console::{Command, CommandWatch};
use teddiebox_core::i2c as bus;
use teddiebox_core::input::{self, Debounced, Edge};
use teddiebox_core::pipe::Pipe;
use teddiebox_core::plate::{Presence, TagEvent, ARRIVALS_TO_AGREE, MISSES_TO_LEAVE};
use teddiebox_core::power::{self, PackState};
use teddiebox_core::sounds::{Announcer, Language, Sound};
use teddiebox_core::tone;
use teddiebox_core::{Action, Core, CoreConfig, Event, TagUid, Unavailable};
use teddiebox_download::{Bytes, ContentSink, Landing, Pages, Placement, Throttle, Writer};
use tlv320dac3100::Tlv320Dac3100;

use crate::index::CardIndex;
use crate::pins::BoardPins;

/// Bytes of heap handed to the radio stack.
///
/// A guess, not a measurement, and the one number here most worth replacing
/// with one. `ControllerConfig::default()` asks the driver for 10 static RX
/// buffers of roughly 1.6 KB each, plus 32 dynamic RX and 32 dynamic TX
/// buffers, so 72 KiB is inside the plausible band and near the bottom of it.
/// Named rather than written inline so that the device plan's measurement has
/// exactly one place to land.
const RADIO_HEAP: usize = 88 * 1024;

// The ESP-IDF-style bootloader identifies an app by this descriptor. Without
// it the image links but no flashing tool will accept it — a failure a build
// gate cannot see.
esp_bootloader_esp_idf::esp_app_desc!();

/// Prints on UART0 so a bench session can tell a running box from a hung one.
#[embassy_executor::task]
async fn heartbeat() {
    let mut ticks: u32 = 0;
    loop {
        esp_println::println!("teddiebox: alive {ticks}");
        ticks = ticks.wrapping_add(1);
        Timer::after(Duration::from_secs(1)).await;
    }
}

/// Reboots into the ROM's UART download mode.
///
/// The ROM checks a bit in the RTC's OPTION1 register as well as the GPIO0
/// strapping pin, so the firmware can ask for download mode on the next reset
/// — no J100 short, no cold power cycle.
///
/// The rails go down first. This is the one reset path the firmware controls,
/// so it is the one that can be tidy about the strapping pin, whatever the
/// board does on the paths it cannot control.
/// Reboots straight back into the application.
///
/// The rails go down first, same as the download path. Exists so a laptop can
/// restart the box without `esptool`, which takes exclusive hold of the serial
/// port and so cannot run while anything is watching the console.
fn reboot(board: &mut BoardPins, gates: &mut Gates) -> ! {
    esp_println::println!("teddiebox: rebooting");
    drain_console();
    board.apply_all(&gates.release_for_reset());
    esp_hal::system::software_reset()
}

/// Lets the last words out before the reset swallows them.
///
/// `software_reset` does not wait for the UART, so everything printed on the
/// way out arrives truncated or not at all — which is exactly the part of a
/// reboot worth reading when something goes wrong during it. Blocking rather
/// than awaiting, because the callers of this never return.
fn drain_console() {
    esp_hal::delay::Delay::new().delay_millis(20);
}

fn reboot_to_download(board: &mut BoardPins, gates: &mut Gates) -> ! {
    esp_println::println!("teddiebox: rebooting into download mode");
    drain_console();
    board.apply_all(&gates.release_for_reset());

    // Hand the USB pads back before rebooting.
    //
    // GPIO19 is the chip's USB D- line, and esp-hal disables the USB pads when
    // that pin becomes the red LED output. `USB_DEVICE.conf0()` survives a
    // software system reset, and the ROM's download mode initialises USB
    // Serial/JTAG as well as UART0 — it announces itself as
    // `DOWNLOAD(USB/UART0)` — so it would come up against pads torn out from
    // under it. Leaving them disabled panics the ROM immediately after
    // `waiting for download`, which is what the first attempt at this did.
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

/// Reports settled presses on the ears and the wake line.
///
/// Also counts the raw flips seen while each change settled: bench step 2 wants
/// the bounce duration of these particular switches measured, and a flip count
/// beside a known poll interval is the cheapest way to see it.
#[embassy_executor::task]
async fn inputs(left: Input<'static>, right: Input<'static>, wake: Input<'static>) {
    const POLL_MS: u64 = 2;

    // Name, debouncer, raw transitions seen since the last settled edge, and
    // the previous raw level. The transition count is the bounce measurement
    // step 2 asks for, so it must count changes of the raw line — counting
    // polls that merely disagree with the settled state yields
    // DEBOUNCE_MS / POLL_MS every single time, which looks like data and is not.
    let mut state = [
        ("left ear", Debounced::released(), 0u32, false),
        ("right ear", Debounced::released(), 0u32, false),
        ("wake", Debounced::released(), 0u32, false),
    ];

    loop {
        let now = Instant::now().as_millis() as u32;
        let raw = [
            input::ear_pressed(left.is_high()),
            input::ear_pressed(right.is_high()),
            input::wake_asserted(wake.is_high()),
        ];

        for ((name, button, transitions, last_raw), &pressed) in state.iter_mut().zip(raw.iter()) {
            if pressed != *last_raw {
                *transitions += 1;
                *last_raw = pressed;
            }

            if let Some(edge) = button.update(pressed, now) {
                let label = match edge {
                    Edge::Pressed => "pressed",
                    Edge::Released => "released",
                };
                // One transition is the change itself; anything above that is
                // bounce. Sampled every POLL_MS, so bounce faster than that is
                // invisible here and reads as a clean edge.
                esp_println::println!("teddiebox: {name} {label}, {transitions} raw transitions");
                *transitions = 0;
            }
        }

        Timer::after(Duration::from_millis(POLL_MS)).await;
    }
}

/// Reports the pack and charger voltages.
///
/// Bench step 3 compares these against a multimeter across a real charge and
/// discharge; until it has, treat them as indicative. The conversion is a
/// straight line through the nominal endpoints and the ESP32-S3's ADC is not
/// linear.
#[embassy_executor::task]
async fn sense(
    mut adc: Adc<'static, esp_hal::peripherals::ADC1<'static>, esp_hal::Blocking>,
    mut battery: esp_hal::analog::adc::AdcPin<
        esp_hal::peripherals::GPIO9<'static>,
        esp_hal::peripherals::ADC1<'static>,
    >,
    mut charger: esp_hal::analog::adc::AdcPin<
        esp_hal::peripherals::GPIO8<'static>,
        esp_hal::peripherals::ADC1<'static>,
    >,
) {
    let mut announcer = Announcer::new();
    loop {
        // Raw counts as well as millivolts. The conversion rests on an
        // assumed attenuation and on GPIO9 measuring the pack rather than
        // something downstream of it, and a millivolt figure alone cannot
        // tell a wrong assumption from a flat battery.
        let pack_raw = adc.read_blocking(&mut battery);
        let charger_raw = adc.read_blocking(&mut charger);
        let pack_mv = power::battery_mv(pack_raw);
        let charger_mv = power::charger_mv(charger_raw);

        let pack_state = power::pack_state(pack_mv);
        let state = match pack_state {
            PackState::Healthy => "healthy",
            PackState::Low => "LOW",
            PackState::Critical => "CRITICAL",
        };
        esp_println::println!(
            "teddiebox: pack {pack_mv} mV ({state}, raw {pack_raw}), charger {charger_mv} mV (raw {charger_raw})"
        );

        // The box says this itself rather than only printing it: a child does
        // not read the console. `Announcer` decides when there is anything
        // worth saying — on the way down, once, and only after several
        // readings agree.
        if let Some(sound) = announcer.observe(pack_state) {
            esp_println::println!("teddiebox: announcing {sound:?}");
            if sound == Sound::BatteryCritical {
                SHUTTING_DOWN.store(true, Ordering::Relaxed);
            }
            SOUND_REQUEST.store(sound.file(), Ordering::Relaxed);
        }

        Timer::after(Duration::from_secs(10)).await;
    }
}

/// Scans the I2C bus once and names what answers.
///
/// Bench step 4. Every device here sits behind power gate 2, so this runs
/// after that rail is up or it finds an empty bus.
fn scan_i2c(i2c: &mut I2c<'_, esp_hal::Blocking>) {
    esp_println::println!("teddiebox: scanning I2C");
    let mut found = 0;

    for address in bus::FIRST_ADDRESS..=bus::LAST_ADDRESS {
        // A zero-length write addresses the device and stops. Anything that
        // acknowledges is present; anything else is not, and the distinction
        // between "absent" and "bus fault" is not one this can draw.
        if i2c.write(address, &[]).is_ok() {
            found += 1;
            match bus::describe(address) {
                Some(name) => esp_println::println!("teddiebox:   {address:#04x} {name}"),
                None => esp_println::println!("teddiebox:   {address:#04x} unexpected"),
            }
        }
    }

    if found == 0 {
        esp_println::println!("teddiebox:   nothing answered — is the rail up?");
    }
}

/// Identifies the accelerometer, then streams its axes.
///
/// Both candidate addresses are tried because 0x18 is shared with the audio
/// codec, which acknowledges and answers something that is not an identity
/// register. Bench step 5 wants tilt traces captured from here as fixtures for
/// the host-side gesture work.
#[embassy_executor::task]
async fn motion(i2c: I2c<'static, esp_hal::Blocking>, mut reset: Output<'static>) {
    // Release the codec from reset before anything on this bus is believed.
    // Held first, deliberately: the part may already be running from a previous
    // boot, and a device half-configured by an earlier session is worse than
    // one that has just come up.
    let hold = board::dac_reset(true);
    let run = board::dac_reset(false);
    reset.set_level(if hold.high { Level::High } else { Level::Low });
    Timer::after(Duration::from_millis(10)).await;
    reset.set_level(if run.high { Level::High } else { Level::Low });
    Timer::after(Duration::from_millis(10)).await;

    let mut bus = i2c;

    // The codec first: it is a one-shot configuration, after which the bus
    // goes back to the accelerometer, which needs it continuously.
    let mut dac = Tlv320Dac3100::new(bus, tlv320dac3100::DEFAULT_ADDRESS);
    let mut dac_delay = esp_hal::delay::Delay::new();
    match dac
        .reset()
        .and_then(|()| codec_bring_up(&mut dac, &mut dac_delay))
    {
        Ok(()) => {
            esp_println::println!("teddiebox: codec configured");
            CODEC_READY.store(true, Ordering::Relaxed);

            // A bring-up listening level, not a design decision — real volume
            // belongs to `teddiebox_core::VolumeModel` and the ears, once
            // there is a box to drive them from.
            //
            // -12 dB was tuned against the test tone, which peaks at half
            // scale. Real content runs to full scale and carries far more
            // spectral energy than a sine, and the first Tonie played through
            // this was, in the listener's words, "120% of the max volume".
            // The codec goes to -63.5 dB, so erring quiet costs nothing and
            // is the right way to err beside someone's head.
            if dac.set_volume_db(BENCH_VOLUME_DB).is_err() {
                esp_println::println!("teddiebox: codec volume not set");
            }

            // What it says it did, rather than what we asked for. A silent
            // output with every configuration register correct is exactly the
            // case this separates: asked wrongly, or declined.
            // Read after `init` has waited out the drivers' ramp. Read before
            // it, HPL reports itself unpowered for 304 ms — measured — which
            // is what "step 6's headphone half is unproven" rested on.
            match dac.power_flags() {
                Ok(f) => esp_println::println!(
                    "teddiebox: codec powered dac_l={} dac_r={} class_d_l={} class_d_r={} hpl={}",
                    f.left_dac,
                    f.right_dac,
                    f.left_class_d,
                    f.right_class_d,
                    f.hpl_driver
                ),
                Err(_) => esp_println::println!("teddiebox: codec flags unreadable"),
            }
        }
        Err(_) => esp_println::println!(
            "teddiebox: codec did not answer — check board::DAC_RESET_RUNS_HIGH"
        ),
    }
    bus = dac.release();
    let mut address = None;

    for candidate in [lis::ADDRESS_SA0_LOW, lis::ADDRESS_SA0_HIGH] {
        let mut probe = Lis3dh::new(bus, candidate);
        let present = matches!(probe.is_present(), Ok(true));
        bus = probe.release();
        if present {
            address = Some(candidate);
            break;
        }
    }

    let Some(address) = address else {
        esp_println::println!("teddiebox: no LIS3DH at 0x18 or 0x19");
        return;
    };

    esp_println::println!("teddiebox: LIS3DH at {address:#04x}");
    let mut accel = Lis3dh::new(bus, address);
    let mut since_report = ACCEL_REPORT_EVERY;
    if accel.init().is_err() {
        esp_println::println!("teddiebox: LIS3DH would not start");
        return;
    }

    loop {
        let speaker = SPEAKER_REQUEST.swap(0, Ordering::Relaxed);
        {
            if let request @ (SPEAKER_MUTE | SPEAKER_UNMUTE) = speaker {
                let bus = accel.release();
                let mut dac = Tlv320Dac3100::new(bus, tlv320dac3100::DEFAULT_ADDRESS);
                let outcome = if request == SPEAKER_UNMUTE {
                    dac.unmute_speaker(&mut dac_delay)
                } else {
                    dac.mute_speaker()
                };
                match outcome {
                    Ok(()) => esp_println::println!(
                        "teddiebox: speaker {}",
                        if request == SPEAKER_UNMUTE {
                            "unmuted"
                        } else {
                            "muted"
                        }
                    ),
                    Err(_) => esp_println::println!("teddiebox: speaker would not change"),
                }
                let bus = dac.release();
                accel = Lis3dh::new(bus, address);
                continue;
            }
        }
        let output = OUTPUT_REQUEST.swap(0, Ordering::Relaxed);
        if let request @ (OUTPUT_DOWN | OUTPUT_UP) = output {
            let bus = accel.release();
            let mut dac = Tlv320Dac3100::new(bus, tlv320dac3100::DEFAULT_ADDRESS);
            let up = request == OUTPUT_UP;
            let outcome = if up {
                dac.start_output(&mut dac_delay)
            } else {
                dac.stop_output()
            };
            match outcome {
                Ok(()) => esp_println::println!(
                    "teddiebox: codec output {}",
                    if up { "up" } else { "down" }
                ),
                Err(_) => esp_println::println!("teddiebox: codec output would not change"),
            }
            let bus = dac.release();
            accel = Lis3dh::new(bus, address);
            continue;
        }
        if CODEC_POWER_DOWN.swap(false, Ordering::Relaxed) {
            let bus = accel.release();
            let mut dac = Tlv320Dac3100::new(bus, tlv320dac3100::DEFAULT_ADDRESS);
            match dac.power_down(&mut dac_delay) {
                Ok(()) => esp_println::println!("teddiebox: codec powered down"),
                Err(_) => esp_println::println!("teddiebox: codec would not power down"),
            }
            let bus = dac.release();
            accel = Lis3dh::new(bus, address);
            continue;
        }
        if CODEC_REINIT.swap(false, Ordering::Relaxed) {
            // Down first, so every run starts from the same place and what is
            // heard is a start-up rather than a re-configuration.
            let bus = accel.release();
            let mut dac = Tlv320Dac3100::new(bus, tlv320dac3100::DEFAULT_ADDRESS);
            let _ = dac.power_down(&mut dac_delay);
            match dac
                .reset()
                .and_then(|()| codec_bring_up(&mut dac, &mut dac_delay))
            {
                Ok(()) => {
                    let _ = dac.set_volume_db(BENCH_VOLUME_DB);
                    match dac.power_flags() {
                        Ok(f) => esp_println::println!(
                            "teddiebox: codec re-inited dac_l={} class_d_l={} hpl={}",
                            f.left_dac,
                            f.left_class_d,
                            f.hpl_driver
                        ),
                        Err(_) => esp_println::println!("teddiebox: codec flags unreadable"),
                    }
                }
                Err(_) => esp_println::println!("teddiebox: codec would not re-init"),
            }
            let bus = dac.release();
            accel = Lis3dh::new(bus, address);
            continue;
        }
        if CODEC_SHUTDOWN.load(Ordering::Relaxed) {
            // The bus went to the accelerometer for good once the codec was
            // configured, so taking the codec down means taking it back.
            let bus = accel.release();
            let mut dac = Tlv320Dac3100::new(bus, tlv320dac3100::DEFAULT_ADDRESS);
            match dac.power_down(&mut dac_delay) {
                Ok(()) => esp_println::println!("teddiebox: codec quiet"),
                Err(tlv320dac3100::Error::StillPowered) => {
                    esp_println::println!("teddiebox: codec output stages still powered")
                }
                Err(_) => esp_println::println!("teddiebox: codec would not power down"),
            }
            CODEC_QUIET.store(true, Ordering::Relaxed);
            // Nothing follows a shutdown but the reset, and the accelerometer
            // no longer owns the bus.
            return;
        }
        // Polled often so a shutdown or a re-init is not held up by an
        // accelerometer nap, but printed rarely: this console is the only
        // user interface the box has, and a reading every 200 ms buries
        // everything else on it.
        if since_report >= ACCEL_REPORT_EVERY {
            since_report = 0;
            match accel.acceleration() {
                Ok([x, y, z]) => esp_println::println!("teddiebox: accel {x} {y} {z}"),
                Err(_) => esp_println::println!("teddiebox: LIS3DH read failed"),
            }
        }
        since_report += 1;
        Timer::after(Duration::from_millis(ACCEL_POLL_MS)).await;
    }
}

/// Applies the start-up sequence with any console overrides in place.
///
/// Both the boot path and `cinit` go through this, so what the bench hears
/// when it re-runs the sequence is what the box will do on its next start —
/// a rig that diverged from the real thing would be worse than no rig.
fn codec_bring_up<I2C, E, D>(
    dac: &mut Tlv320Dac3100<I2C>,
    delay: &mut D,
) -> Result<(), tlv320dac3100::Error<E>>
where
    I2C: embedded_hal::i2c::I2c<Error = E>,
    D: embedded_hal::delay::DelayNs,
{
    let mut analog = [(0u8, 0u8, 0u8); 32];
    let mut dac_table = [(0u8, 0u8, 0u8); 8];
    let analog = codec_apply_overrides(tlv320dac3100::INIT_ANALOG, &mut analog);
    let dac_regs = codec_apply_overrides(tlv320dac3100::INIT_DAC, &mut dac_table);
    dac.apply(delay, analog, dac_regs)
}

/// How often the accelerometer is read.
///
/// Short because this loop is also where a shutdown and a codec re-init are
/// noticed, and a reboot waiting on a two-second nap feels broken.
const ACCEL_POLL_MS: u64 = 200;
/// One reading printed per ten polls, so the console stays legible.
const ACCEL_REPORT_EVERY: u32 = 10;

/// How loud the box plays during bring-up.
///
/// Deliberately low. Turning it up is a one-line change and a reflash; the
/// alternative is discovering it is too loud with the box against your ear.
const BENCH_VOLUME_DB: i8 = -35;

/// What the console has asked the media task to do.
///
/// One word rather than a flag each, because the tone, the card walk and WAV
/// playback all contend for the same two pieces of hardware — the I2S
/// peripheral and the SD bus — and a task that owns both is the honest way to
/// say that. None of them runs at boot: the tone is loud, the walk powers a
/// rail shared with the NFC reader, and the box is usually sitting next to
/// whoever is working on it.
static REQUEST: AtomicU8 = AtomicU8::new(REQUEST_NONE);
const REQUEST_NONE: u8 = 0;
const REQUEST_TONE: u8 = 1;
const REQUEST_WALK: u8 = 2;
const REQUEST_WAV: u8 = 3;
const REQUEST_TAF: u8 = 4;
const REQUEST_CONTENT: u8 = 5;
const REQUEST_PCM: u8 = 6;
const REQUEST_CACHE: u8 = 7;

/// What the console has asked the NFC reader to do.
///
/// Separate from [`REQUEST`] because the reader has its own SPI bus and shares
/// only the power rail, so it has no reason to queue behind a track that is
/// still playing.
static NFC_REQUEST: AtomicU8 = AtomicU8::new(REQUEST_NONE);
const NFC_INVENTORY: u8 = 1;
const NFC_UNLOCK: u8 = 2;
const NFC_FORCE_UNLOCK: u8 = 3;
const NFC_LOCK: u8 = 4;
const NFC_READ_MEMORY: u8 = 5;
const NFC_READ_TOKEN: u8 = 6;

/// The tag's memory, kept to spend on a download.
///
/// RAM only, gone at the next reset, and never printed — the same treatment the
/// SLIX password and the Wi-Fi passphrase get, for the same reason. It is what
/// the tonies cloud accepts in place of proof that this box owns this figure.
///
/// Written only by the console `token` command, and read only by the console
/// `get` command, at the moment it builds its own [`FetchRequest`]. The
/// plate's fetch never touches this static: its token travels with its ruid
/// in [`FETCH_REQUEST`] instead, so a `token` typed at the bench in between
/// cannot attach itself to a fetch it was never read for.
static TAG_TOKEN: CsMutex<RefCell<Option<[u8; 32]>>> = CsMutex::new(RefCell::new(None));

/// Set when the console asks the radio for a scan.
///
/// Its own signal rather than a variant of `NFC_REQUEST`: the radio and the
/// reader share nothing but the console that drives them.
static NET_REQUEST: AtomicU8 = AtomicU8::new(REQUEST_NONE);
const NET_SCAN: u8 = 1;
const NET_UP: u8 = 2;
const NET_DOWN: u8 = 3;
const NET_STATUS: u8 = 4;
const NET_TLS: u8 = 5;
const NET_GET: u8 = 6;

/// A figure's identifier and the token that authorises fetching its story.
///
/// The two travel together for the same reason [`PlateState::Present`] keeps
/// its uid and token in one variant rather than two statics: sent as a pair,
/// "this figure's fetch, that figure's token" has no representation. Split
/// into a ruid static beside a token static, it would — a write to one
/// between the other's write and the fetch reading both is exactly how one
/// figure's fetch went out authorised with a different figure's token.
#[derive(Debug, Clone, Copy)]
struct FetchRequest {
    /// The identifier in the byte order `get`'s command line and the fetch's
    /// request line both use — the reverse of the order the reader hands a
    /// UID over in.
    ruid: u64,
    token: Option<[u8; 32]>,
}

/// The fetch about to be raised on [`NET_GET`], written whole in one go.
///
/// Two writers — the console `get` command and the plate's `RequestContent`
/// action — and each writes its own [`FetchRequest`] immediately before
/// raising the request, never touching the other's fields separately.
static FETCH_REQUEST: CsMutex<RefCell<Option<FetchRequest>>> = CsMutex::new(RefCell::new(None));

/// Bytes waiting to move from the network to the card.
///
/// The two ends run in different tasks and neither may wait for the other: a
/// media loop that awaited the network would underrun the moment the server
/// paused, and a network task that awaited the card would stall the socket
/// behind a decode. This absorbs the difference.
///
/// Sized against the measured download rate. At ~47 KB/s the media task's idle
/// poll leaves about 4.7 KB between drains, so eight kibibytes keeps the socket
/// moving even when a drain is late. A full pipe is back-pressure, not a fault.
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

/// Where this download starts, or [`NOTHING_TO_FETCH`] when the card has it all.
///
/// Written by the card's owner during [`DOWNLOAD_PREPARING`] and read by the
/// producer, which cannot touch the card itself.
static DOWNLOAD_FROM: AtomicU32 = AtomicU32::new(0);

/// `DOWNLOAD_FROM`'s answer for "there is nothing to ask the server for".
///
/// A sentinel rather than another flag because it is the same question — where
/// does this download start — and `u32::MAX` is not a plausible offset in a
/// file the card could hold.
const NOTHING_TO_FETCH: u32 = u32::MAX;

/// What the sidecar says a resume should be validated against.
static DOWNLOAD_RESUME_ETAG: CsMutex<RefCell<Option<teddiebox_cloud::ETag>>> =
    CsMutex::new(RefCell::new(None));

/// How many bytes the content file held when the download was planned.
static DOWNLOAD_ON_CARD: AtomicU32 = AtomicU32::new(0);

/// How long the sidecar promised the whole file would be, or zero for none.
///
/// Kept so the writer can tell a resume of *this* file from a resume against a
/// file the server has since replaced, which is otherwise indistinguishable.
static DOWNLOAD_EXPECT: AtomicU32 = AtomicU32::new(0);

/// Where the server said this body belongs. Zero means it declined the range.
static DOWNLOAD_AT: AtomicU32 = AtomicU32::new(0);

/// How long the server says the whole file is, or zero for "it did not say".
///
/// The card's owner writes the sidecar and is on the far side of the pipe from
/// the response head, so the length has to be left here for it to find.
static DOWNLOAD_TOTAL: AtomicU32 = AtomicU32::new(0);

/// What a later resume can be validated against, when the server offered one.
static DOWNLOAD_ETAG: CsMutex<RefCell<Option<teddiebox_cloud::ETag>>> =
    CsMutex::new(RefCell::new(None));
/// Set when the consumer cannot write, so the producer stops rather than
/// wedging against a pipe nobody is draining.
static DOWNLOAD_ABORT: AtomicBool = AtomicBool::new(false);

/// Which figure the fetch that is running — or that most recently ended — is
/// for.
///
/// Set once, from the [`FetchRequest`] that was read to start it, at the same
/// moment [`DOWNLOAD_DIR`] and [`DOWNLOAD_FILE`] are set. Read by
/// [`fetch_ended`] so an outcome always carries the identity of the fetch
/// that produced it, never whatever the newest queued request happens to be.
static FETCH_ACTIVE_RUID: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);

/// What the last download came to, for whoever asked for it.
///
/// The fetch runs in the network task and ends on the card in the media task,
/// while the only thing that can decide what to do about it — the reducer —
/// lives in the media task's loop. So the answer is left here rather than
/// returned: it crosses two task boundaries and neither can call the other.
///
/// Deliberately coarse. `Unavailable` has two words for failure and the
/// console keeps the real one; this carries only what the reducer can act on.
static FETCH_OUTCOME: AtomicU8 = AtomicU8::new(FETCH_NOTHING);
/// No download has ended since the last one was read.
const FETCH_NOTHING: u8 = 0;
/// The bytes are on the card.
const FETCH_COMPLETED: u8 = 1;
/// The server could not be reached, or could not be asked.
const FETCH_UNREACHABLE: u8 = 2;
/// The server was reached and has nothing filed under that figure.
const FETCH_NO_CONTENT: u8 = 3;

/// Which figure [`FETCH_OUTCOME`] belongs to.
///
/// Set alongside it, by the same call, so the media task can tell its own
/// fetch's answer apart from a console `get` that happened to finish while a
/// figure — the same one or a different one — sits on the plate.
static FETCH_OUTCOME_RUID: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);

/// Records how a download ended, once.
///
/// A download ends in one of several places — before the request, in the
/// fetch, or at the card — and every one of them has to leave the same
/// answer, attributed to [`FETCH_ACTIVE_RUID`]: whichever fetch was actually
/// running when it happened.
fn fetch_ended(outcome: u8) {
    FETCH_OUTCOME_RUID.store(FETCH_ACTIVE_RUID.load(Ordering::Relaxed), Ordering::Relaxed);
    FETCH_OUTCOME.store(outcome, Ordering::Relaxed);
}

/// Which of the reducer's two words a failed fetch deserves.
///
/// A `404` or a `403` means the server answered and has no story for this
/// figure; teddyCloud's `403` is what a request without a usable token gets,
/// which is the same thing from the box's point of view. Everything else —
/// the name, the socket, the handshake, a fault of the server's own — means
/// the server was not reached, whatever the reason. The console keeps the
/// reason; this is only what the box can say out loud.
fn why_unavailable(error: &tls::Error) -> Unavailable {
    match error {
        tls::Error::NoContent
        | tls::Error::Cloud(teddiebox_cloud::CloudError::UnexpectedStatus(403 | 404)) => {
            Unavailable::NoContent
        }
        _ => Unavailable::Unreachable,
    }
}

/// Set while audio is being fed, so the download can leave the radio alone.
static PLAYING: AtomicBool = AtomicBool::new(false);

/// How far ahead the download is of whatever is waiting on it.
///
/// Nothing streams yet: `get` fetches a file nobody is playing, so no decoder
/// can run out and the honest answer is "nobody is waiting". Saying that in the
/// throttle's own vocabulary — an unreachable lead — keeps one concept rather
/// than adding a second. When playback and download meet on the same file this
/// becomes the watermark minus the decoder's position, and the thresholds below
/// start earning their keep.
const NOTHING_WAITING: Pages = Pages(u32::MAX);
/// Fetch again once the decoder is within this many pages of the write head.
const RESUME_BELOW: Pages = Pages(8);
/// Stop once it is this far ahead. The gap between the two is what stops the
/// radio starting and stopping on every page the decoder consumes.
const PAUSE_ABOVE: Pages = Pages(32);

/// A download being written to the card.
///
/// The card is deliberately not in here. It is handed to `service_download`
/// for one call at a time, so that a download in flight never holds a borrow
/// the media loop needs for anything else; what has to outlive a call is the
/// counting, and that is all this keeps.
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
        // Reported rather than returned. A failed flush is not fatal to the
        // download — the bytes are on the card, only the recorded length is
        // behind — so it costs a longer resume rather than a broken file, and
        // returning it would stop a download that is still perfectly able to
        // continue.
        if let Err(reason) = self.card.flush(self.file) {
            esp_println::println!("teddiebox: get flush failed — {reason}");
        }
        Ok(())
    }
}

/// How much of a download may be in flight before it is made durable.
///
/// This is what makes an interrupted download worth resuming rather than
/// merely worth deleting: `embedded-sdmmc` only makes a file's recorded length
/// truthful at a flush, so bytes written since the last one are invisible to
/// the next boot however safely they reached the card. Without this the answer
/// to "how far did we get" is always zero.
///
/// A megabyte is roughly twenty-four seconds at the rate this box downloads —
/// little enough to lose, seldom enough not to spend the card's life on flushes.
const FLUSH_EVERY: u32 = 1 << 20;

/// Asks the card what it already has, and leaves the answer for the producer.
///
/// This runs here because only this task may touch the card, and it runs
/// *before* the request because what the card holds is what the request has to
/// ask for. A box with no card plans a whole download: the failure then comes
/// from the write, which says so, rather than from a resume against a file
/// nobody could read.
fn plan_download(card: Option<&storage::Mounted>) {
    let dir = DOWNLOAD_DIR.load(Ordering::Relaxed);
    let file = DOWNLOAD_FILE.load(Ordering::Relaxed);
    critical_section::with(|cs| *DOWNLOAD_RESUME_ETAG.borrow_ref_mut(cs) = None);

    let mut from = 0;
    let mut on_card = 0;
    let mut expected = 0;

    if let Some(card) = card {
        let length_on_card = card.cache_length(dir, file);
        on_card = length_on_card.unwrap_or(0);

        let mut buffer = [0u8; teddiebox_download::MAX_SIDECAR];
        let sidecar = card
            .read_sidecar(dir, file, &mut buffer)
            .and_then(|filled| core::str::from_utf8(&buffer[..filled]).ok())
            .and_then(|text| teddiebox_download::Sidecar::parse(text).ok());

        expected = sidecar.as_ref().map_or(0, |held| held.length);
        let cached = teddiebox_download::Cached {
            sidecar,
            length_on_card,
        };
        match teddiebox_download::decide(&cached) {
            teddiebox_download::Decision::Play => from = NOTHING_TO_FETCH,
            teddiebox_download::Decision::Fetch => {}
            teddiebox_download::Decision::Resume { from: at, etag } => {
                from = at;
                critical_section::with(|cs| *DOWNLOAD_RESUME_ETAG.borrow_ref_mut(cs) = etag);
            }
        }
    }

    DOWNLOAD_FROM.store(from, Ordering::Relaxed);
    DOWNLOAD_ON_CARD.store(on_card, Ordering::Relaxed);
    DOWNLOAD_EXPECT.store(expected, Ordering::Relaxed);
    DOWNLOAD_STATE.store(DOWNLOAD_PLANNED, Ordering::Relaxed);
}

/// Records how long the file beside it is meant to be.
///
/// Best effort on purpose. A content file with no sidecar is incomplete by
/// definition, so a failure here costs a refetch — where a download that
/// pressed on and later *looked* complete would cost a story that stops in the
/// middle. Silence when the server gave no length is the same bargain: nothing
/// is claimed that was not said.
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
/// Called from the media task's idle poll, and it **never waits**: it writes
/// what is in the pipe and returns. That is what keeps the card's owner free to
/// answer a play request while a download is in flight — the thing that makes
/// downloading and playing at the same time possible at all.
///
/// Returns whether a download is still in flight, so the caller can poll
/// faster while one is.
fn service_download(card: Option<&storage::Mounted>, write: &mut Option<CacheWrite>) -> bool {
    let state = DOWNLOAD_STATE.load(Ordering::Relaxed);
    match state {
        DOWNLOAD_IDLE => return false,
        // Both are brief and neither writes anything: one answers the
        // producer, the other waits for a response head that has not arrived.
        // Reported as busy so the loop keeps its fast cadence across them.
        DOWNLOAD_PREPARING => {
            plan_download(card);
            return true;
        }
        DOWNLOAD_PLANNED => return true,
        _ => {}
    }

    if write.is_none() {
        // A request that failed before the head arrived handed over nothing.
        // Opening the file here would create — or truncate — a cache entry on
        // behalf of a download that never asked the server for a byte.
        if state == DOWNLOAD_ENDED && DOWNLOAD_SENT.load(Ordering::Relaxed) == 0 {
            DOWNLOAD_STATE.store(DOWNLOAD_IDLE, Ordering::Relaxed);
            return false;
        }

        let dir = DOWNLOAD_DIR.load(Ordering::Relaxed);
        let file = DOWNLOAD_FILE.load(Ordering::Relaxed);
        // Where the server put this body decides whether the partial file on
        // the card is continued or thrown away, and getting that backwards
        // splices the start of a story onto its middle at exactly the right
        // length for nothing downstream to notice.
        let expected = DOWNLOAD_EXPECT.load(Ordering::Relaxed);
        let total = DOWNLOAD_TOTAL.load(Ordering::Relaxed);
        let placement = teddiebox_download::place(&Landing {
            offset: DOWNLOAD_AT.load(Ordering::Relaxed),
            length_on_card: DOWNLOAD_ON_CARD.load(Ordering::Relaxed),
            // Zero is this firmware's "nothing was said", which is what the
            // crate spells `None`.
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
                        // Only when starting over. A resumed download is
                        // continuing the file this sidecar already vouches
                        // for, and rewriting it would claim the server said
                        // something about a file it was only asked for part of.
                        write_sidecar(card, dir, file);
                    }
                }
                *write = Some(CacheWrite {
                    file: handle,
                    // From zero even for a resume: what this counts is
                    // measured against what the producer has sent this time
                    // round, not against the length of the file on the card.
                    writer: Writer::resuming(0, FLUSH_EVERY),
                    crc: teddiebox_core::checksum::Crc32::new(),
                });
            }
            Err(reason) => {
                // Abort rather than let the pipe fill: a producer waiting on a
                // sink nobody drains never returns, and the box would look
                // hung rather than broken.
                esp_println::println!("teddiebox: get cannot write — {reason}");
                // The story cannot be obtained, which is all the reducer has a
                // word for. Which of the two it is told is a judgement: a card
                // that will not take the bytes is not the server's fault, but
                // "reached and has nothing" would be a lie, and `Unreachable`
                // at least sends the box back to the network next time.
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

    // Done only when the producer has stopped *and* everything it handed over
    // has reached the card. Stopping at the first of those would truncate the
    // file by whatever was still in the pipe.
    let sent = DOWNLOAD_SENT.load(Ordering::Relaxed);
    if state == DOWNLOAD_ENDED && active.writer.watermark() >= Bytes(sent) {
        // Whatever this reports the sink has already said; there is nothing
        // here that could act on it.
        let _ = active.writer.finish(&mut sink);
        esp_println::println!(
            "teddiebox: get wrote {} bytes, crc32 {:08X}",
            active.writer.watermark().0,
            active.crc.finish()
        );
        card.close_file(active.file);
        *write = None;
        // Here rather than where the fetch returned: a story is ready when it
        // is on the card, not when the last byte left the socket. Playing on
        // the earlier answer would open a file the writer has not finished.
        fetch_ended(FETCH_COMPLETED);
        DOWNLOAD_STATE.store(DOWNLOAD_IDLE, Ordering::Relaxed);
        DOWNLOAD_ABORT.store(false, Ordering::Relaxed);
        return false;
    }
    true
}

/// Credentials typed at the bench.
///
/// RAM only, gone at the next reset — the same treatment the SLIX password
/// gets and for the same reason: a credential belongs neither in the image nor
/// in the repository. **This is a stop-gap.** The design has these arriving
/// from the card's `CONFIG.TXT`, which is why `net::Radio::acquire` takes
/// a whole `Config` rather than two strings: when the card hands one over,
/// this static goes away and `net.rs` does not change at all.
static CONFIGURATION: CsMutex<RefCell<Config>> = CsMutex::new(RefCell::new(Config {
    ssid: String::new(),
    password: String::new(),
    server: String::new(),
    insecure: false,
}));

/// How long to wait for a DHCP lease before calling it a failure.
///
/// Association succeeding and addressing failing are different faults with
/// different causes, so they are waited for — and reported — separately.
const DHCP_TIMEOUT: Duration = Duration::from_secs(20);

/// What the bench has typed, so the card cannot undo it.
static OVERRIDDEN: CsMutex<RefCell<Overridden>> = CsMutex::new(RefCell::new(Overridden {
    ssid: false,
    password: false,
    server: false,
    insecure: false,
}));

fn set_ssid(value: String<MAX_SSID>) {
    critical_section::with(|cs| {
        CONFIGURATION.borrow_ref_mut(cs).ssid = value;
        OVERRIDDEN.borrow_ref_mut(cs).ssid = true;
    });
}

fn set_password(value: String<MAX_PASSPHRASE>) {
    critical_section::with(|cs| {
        CONFIGURATION.borrow_ref_mut(cs).password = value;
        OVERRIDDEN.borrow_ref_mut(cs).password = true;
    });
}

fn set_insecure(value: bool) {
    critical_section::with(|cs| {
        CONFIGURATION.borrow_ref_mut(cs).insecure = value;
        OVERRIDDEN.borrow_ref_mut(cs).insecure = true;
    });
}

/// Publishes what the card said.
///
/// The media task owns the card and calls this once at boot; the console
/// writes over it afterwards, which is what makes a mistyped card
/// diagnosable at the bench without pulling it.
fn set_configuration(value: Config) {
    critical_section::with(|cs| {
        let overridden = *OVERRIDDEN.borrow_ref(cs);
        let mut held = CONFIGURATION.borrow_ref_mut(cs);
        *held = overridden.merge(value, &held);
    });
}

/// The credentials shaped the way the radio wants them.
///
/// `None` if either half is missing, because handing the driver an empty
/// string would fail association in a way that looks like a wrong password.
fn credentials() -> Option<Config> {
    critical_section::with(|cs| {
        let held = CONFIGURATION.borrow_ref(cs);
        if held.ssid.is_empty() || held.password.is_empty() {
            return None;
        }
        Some(held.clone())
    })
}

/// Access points one scan will report.
///
/// **16 is a measured ceiling, not a preference.** Raising it to 48 on
/// 2026-09-03 wedged the scan permanently: `scan_async` was entered and never
/// returned, on every attempt. The other tasks kept running — the heartbeat
/// never missed — so only the radio task was lost, which is what made it look
/// at first like the whole box had died. 16 has scanned cleanly many times
/// either side of that experiment. The boundary between them has not been
/// looked for, and `RADIO_HEAP` is the first thing to suspect: this is the
/// only code that allocates, and 72 KiB was always a guess.
///
/// The cost of 16 is real. The driver fills the quota **in channel order, not
/// by signal strength**, so in a crowded band it can stop before reaching a
/// high channel — at this bench it filled up across channels 1 to 8 and never
/// reached channel 11, where the access point the box was sitting next to at
/// -29 dBm lives. A truncated list is therefore reported as truncated, because
/// a survey that silently omits the strongest signal in the room is worse than
/// one that admits it stopped early.
const SCAN_LIMIT: usize = 16;

/// How long a scan may take before it is called a failure.
///
/// `scan_async` waits for the driver to raise `ScanDone`, and a radio that
/// never initialises raises nothing. `net.rs` deliberately holds no timeouts,
/// so the bench command owns this one.
///
/// **It bounds less than it looks like it does.** Racing it with `select`
/// only helps while the scan future actually yields: `select` re-checks the
/// timer when the task is polled, so a future that blocks *inside* its own
/// `poll` is never interrupted by it. That is exactly the failure seen at
/// `SCAN_LIMIT` 48 — the timer expired and nothing was printed, because the
/// task never came back to look. This catches a scan that waits forever for
/// an event; it cannot catch one that never yields.
const SCAN_TIMEOUT: Duration = Duration::from_secs(15);

/// The SLIX privacy password, as typed at the console.
///
/// RAM only, and deliberately: it is a credential, it is never written to the
/// card or committed, and it dies with the next reset.
static NFC_PASSWORD: AtomicU32 = AtomicU32::new(0);

/// The block range a pending `mem` carries: first block in the high byte,
/// block count in the low one.
///
/// Packed into one word rather than kept in two, so the reader task cannot
/// observe a first block from one command beside a count from the next.
static NFC_MEM_RANGE: AtomicU32 = AtomicU32::new(0);

/// Whether the reader polls the plate on its own. Off at boot.
///
/// A poller that unlocks tags by itself would contaminate any bench
/// measurement that involves a figure, so this stays off until `plate on`
/// asks for it in a session that is watching.
static PLATE_POLLING: AtomicBool = AtomicBool::new(false);
/// Asks the reader task to print the slowest reply it has seen. Set when
/// polling is switched off, because that is when a run is over.
static PLATE_REPORT: AtomicBool = AtomicBool::new(false);

/// What is on the plate right now — the current state, not a queue of edges.
///
/// A `Signal` rather than a channel because the media task can be away for as
/// long as a decode takes, and a queue would need a depth nobody can justify.
/// Holding the latest state instead is idempotent and cannot overflow; the
/// cost is that a figure placed and lifted inside one media pass is invisible,
/// which is the right answer anyway.
static PLATE_TAG: Signal<CriticalSectionRawMutex, PlateState> = Signal::new();

/// The token travels with the UID it was read from. As two separate statics
/// it would be possible to send one figure's token for another's story.
#[derive(Debug, Clone, Copy)]
enum PlateState {
    Present {
        uid: [u8; 8],
        token: Option<[u8; 32]>,
    },
    Absent,
}

/// How often the reader looks, when it is looking at all — counted in the
/// `nfc_reader` loop's existing 100 ms ticks, so five is half a second.
///
/// Provisional and uncalibrated. Not a free parameter: every poll is a full
/// transaction with the field up, this pack has no protection circuit, and the
/// one unexplained brownout in this project happened while transmitting into a
/// coupled tag. A poller is the first thing that transmits on a schedule
/// rather than when a person asks.
const PLATE_POLL_TICKS: u8 = 5;

/// The language this box speaks, from `TEDDIEBOX_LANGUAGE` in `.envrc.local`.
///
/// Chosen at build time rather than read off the card: it is a property of the
/// box, and the card carries all four languages regardless. An unset value
/// takes the German sounds; a value that is not a language fails the build,
/// because a box quietly speaking the wrong language to a child is not a
/// failure anyone would look for.
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
static ANNOUNCING: AtomicBool = AtomicBool::new(false);

/// Set once the codec is configured and would be heard if it were driven.
///
/// The start-up jingle waits on this. `init` spends 400 ms letting the output
/// drivers ramp, and a jingle that begins before then loses its first second
/// to a codec that is not listening yet.
static CODEC_READY: AtomicBool = AtomicBool::new(false);

/// Set when the box has said it is turning off, and must therefore do it.
///
/// `BatteryCritical` is not a warning, it is an announcement — "battery is
/// critical, turning off now" — so it is the one sound with an obligation
/// attached. A box that says this and keeps playing has told a child
/// something untrue, which is worse than saying nothing.
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

/// Which content file `play` names, as two halves of `CONTENT/<dir>/<file>`.
static CONTENT_DIRECTORY: AtomicU32 = AtomicU32::new(0);
static CONTENT_FILE: AtomicU32 = AtomicU32::new(0);

/// How many frames `pcm` should print.
static PCM_FRAMES: AtomicU8 = AtomicU8::new(0);

/// Asked for before the rails go down, answered when the codec is quiet.
///
/// The class-D amplifier is powered and unmuted for the whole of a session,
/// and `rb` cut its supply out from under it — which is a click into the
/// speaker beside a child's head. The codec can take itself down cleanly, but
/// only over I2C, and that bus belongs to `motion`; hence a request rather
/// than a call.
static CODEC_SHUTDOWN: AtomicBool = AtomicBool::new(false);
static CODEC_QUIET: AtomicBool = AtomicBool::new(false);

/// Ask the motion task to take the codec down and bring it back up.
static CODEC_REINIT: AtomicBool = AtomicBool::new(false);

/// Ask the motion task to run the codec's power-down, and nothing else.
static CODEC_POWER_DOWN: AtomicBool = AtomicBool::new(false);

/// A pending speaker mute change: 0 nothing, 1 mute, 2 unmute.
static SPEAKER_REQUEST: AtomicU8 = AtomicU8::new(0);
const SPEAKER_MUTE: u8 = 1;
const SPEAKER_UNMUTE: u8 = 2;

/// A pending output power change: 0 nothing, 1 down, 2 up.
static OUTPUT_REQUEST: AtomicU8 = AtomicU8::new(0);
const OUTPUT_DOWN: u8 = 1;
const OUTPUT_UP: u8 = 2;

/// Register overrides applied to the codec's start-up sequence.
///
/// Which register value stops the box clicking on start-up is a question for
/// the ear, and a reflash between guesses makes that loop minutes long. Each
/// slot is packed as `page << 16 | register << 8 | value`, with the top byte
/// set to mark it used, so the whole table is lock-free and needs no
/// allocator.
static CODEC_OVERRIDES: [AtomicU32; CODEC_OVERRIDE_SLOTS] =
    [const { AtomicU32::new(0) }; CODEC_OVERRIDE_SLOTS];
const CODEC_OVERRIDE_SLOTS: usize = 6;
const CODEC_OVERRIDE_USED: u32 = 1 << 24;

fn codec_override_set(page: u8, register: u8, value: u8) -> bool {
    let packed = CODEC_OVERRIDE_USED
        | (u32::from(page) << 16)
        | (u32::from(register) << 8)
        | u32::from(value);
    let same_register = |slot: u32| slot & 0x00FF_FF00 == packed & 0x00FF_FF00;
    // Replace an override of the same register rather than filling the table
    // with a history of one register's values.
    for slot in CODEC_OVERRIDES.iter() {
        let current = slot.load(Ordering::Relaxed);
        if current & CODEC_OVERRIDE_USED != 0 && same_register(current) {
            slot.store(packed, Ordering::Relaxed);
            return true;
        }
    }
    for slot in CODEC_OVERRIDES.iter() {
        if slot.load(Ordering::Relaxed) & CODEC_OVERRIDE_USED == 0 {
            slot.store(packed, Ordering::Relaxed);
            return true;
        }
    }
    false
}

fn codec_overrides_clear() {
    for slot in CODEC_OVERRIDES.iter() {
        slot.store(0, Ordering::Relaxed);
    }
}

/// Copies `table` into `out`, applying any override for each register.
fn codec_apply_overrides<'a>(
    table: &[(u8, u8, u8)],
    out: &'a mut [(u8, u8, u8)],
) -> &'a [(u8, u8, u8)] {
    out[..table.len()].copy_from_slice(table);
    for entry in out[..table.len()].iter_mut() {
        for slot in CODEC_OVERRIDES.iter() {
            let packed = slot.load(Ordering::Relaxed);
            if packed & CODEC_OVERRIDE_USED == 0 {
                continue;
            }
            let page = ((packed >> 16) & 0xFF) as u8;
            let register = ((packed >> 8) & 0xFF) as u8;
            if entry.0 == page && entry.1 == register {
                entry.2 = (packed & 0xFF) as u8;
            }
        }
    }
    &out[..table.len()]
}

/// How long a reboot waits for the codec before going ahead regardless.
///
/// Going ahead is the right failure: a box that will not reboot is worse than
/// one that clicks, and the bench needs `rb` to work even when the codec does
/// not answer.
const CODEC_SHUTDOWN_TIMEOUT_MS: u64 = 800;

/// Asks the codec to go quiet, and waits until it has or the wait runs out.
async fn quieten_codec() {
    CODEC_SHUTDOWN.store(true, Ordering::Relaxed);
    let deadline = Instant::now() + Duration::from_millis(CODEC_SHUTDOWN_TIMEOUT_MS);
    while !CODEC_QUIET.load(Ordering::Relaxed) {
        if Instant::now() > deadline {
            esp_println::println!("teddiebox: codec did not go quiet in time");
            return;
        }
        Timer::after(Duration::from_millis(5)).await;
    }
}

/// Owns the I2S peripheral and the SD bus, and serves the bench commands that
/// need them.
///
/// One task for three jobs because they contend for the same two pieces of
/// hardware. The card is mounted once and kept, so `sd` and `wav` do not each
/// re-identify it and re-raise the bus clock.
///
/// The tone and WAV playback are both terminal: each takes the I2S peripheral
/// and does not give it back — the tone by design, since it repeats forever,
/// and playback because its DMA buffer is a `static` claimed once whose
/// pre-fill state this does not re-derive. `rb` restarts the box, which is the
/// documented way to run another.
/// How much room the card's configuration file is given.
///
/// A read that fills this is refused rather than parsed — see
/// [`teddiebox_config::Config::parse_read`] — so this is the file-size limit,
/// not a hint. The file as written is under a kilobyte; the rest is room for
/// somebody to add comments without the box quietly disagreeing about what
/// they configured.
const CONFIG_BUFFER: usize = 2048;

/// Reads the box's certificate and private key off the card, if they are there.
///
/// Optional by design: without them the box can still fetch anything the server
/// already holds, which is most of what a bench does. They are what let
/// teddyCloud tell *which* box is asking, and so what lets it fetch a figure it
/// has no copy of.
///
/// Nothing about the key is printed but its length.
fn read_identity(card: &storage::Mounted) {
    let mut certificate = [0u8; tls::CERT_BYTES];
    let mut key = [0u8; tls::CERT_BYTES];

    // The authority first, and separately: verifying the server is useful even
    // on a box that cannot prove who it is, and the two failures want different
    // words.
    match card.read_certificate("TCCA.DER", &mut certificate) {
        Ok(n) if tls::set_anchor(&certificate[..n]) => {
            esp_println::println!("teddiebox: identity server verified against a {n} byte CA")
        }
        Ok(_) => esp_println::println!("teddiebox: identity CA already set"),
        Err(reason) => esp_println::println!(
            "teddiebox: identity no CA — {reason}; the server cannot be verified, \
             so `insecure = yes` is the only way it will connect"
        ),
    }

    let read = card
        .read_certificate("CLIENT.DER", &mut certificate)
        .and_then(|c| {
            card.read_certificate("PRIVATE.DER", &mut key)
                .map(|k| (c, k))
        });

    match read {
        Ok((c, k)) => {
            if tls::set_identity(&certificate[..c], &key[..k]) {
                // The certificate's length, never the key's contents.
                esp_println::println!("teddiebox: identity {c} byte certificate, {k} byte key");
            } else {
                esp_println::println!("teddiebox: identity already set");
            }
        }
        Err(reason) => esp_println::println!(
            "teddiebox: identity none — {reason}; the server will not know which box is asking"
        ),
    }
}

/// Reads `CONFIG.TXT` the first time the card is available, and says what it
/// found.
///
/// **A card that cannot be configured does not stop the box.** Everything
/// already on it still plays; only the network stays down. A typo in a text
/// file must never cost a child their story, and the box is a toy before it is
/// a network client.
///
/// It says so twice, because the two audiences are different: a line on the
/// console for whoever has a cable, and the box's own words for whoever does
/// not.
fn read_configuration_once(card: &storage::Mounted, done: &mut bool) {
    if *done {
        return;
    }
    *done = true;

    read_identity(card);

    let mut buffer = [0u8; CONFIG_BUFFER];
    match card.read_config(&mut buffer) {
        Ok(config) => {
            // The passphrase is not printed, here or anywhere. Its length is
            // enough to tell a truncated card from a wrong one.
            esp_println::println!(
                "teddiebox: config ssid {}, server {}, passphrase {} chars{}",
                config.ssid,
                config.server,
                config.password.len(),
                if config.insecure {
                    ", certificates NOT checked"
                } else {
                    ""
                }
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

/// A tag's UID in the byte order `get`'s ruid argument — and a fetch's
/// request line — both use.
///
/// The reader hands the UID over in the opposite order. Both writers of a
/// [`FetchRequest`] and the outcome comparison in the media loop go through
/// this one conversion rather than repeating the reversal at each call site.
fn ruid_of(tag: TagUid) -> u64 {
    let mut bytes = tag.0;
    bytes.reverse();
    u64::from_be_bytes(bytes)
}

/// Carries out one of the reducer's decisions.
///
/// Every arm reaches a path a console command already takes, by setting the
/// same statics that command sets. Nothing new is built here: what this adds
/// is that the box can now reach those paths without anybody typing.
///
/// `token` is the credential that came with the figure currently on the
/// plate, kept by the caller rather than in a shared static — see
/// [`FetchRequest`] for why splitting it from the tag it belongs to is the
/// mistake this whole path exists to avoid.
fn perform(action: Action, index: &CardIndex<'_>, token: Option<[u8; 32]>) {
    match action {
        Action::Play { tag, from } => {
            let path = teddiebox_download::content_path(tag.0);
            // A shipped story lives under `CONTENT/` and a downloaded one under
            // `CACHE/`, and neither request looks in the other's directory. The
            // index has just answered this very question to decide the story
            // was there at all; asking it again is what turns that into a path.
            let request = if index.on_stock_card(path.directory, path.file) {
                REQUEST_CONTENT
            } else {
                REQUEST_CACHE
            };
            if from.page != 0 {
                // Nothing resumes yet — `saved_position` answers zero for every
                // figure — so a page here would mean the index grew a memory
                // this never learned to honour.
                esp_println::println!("teddiebox: plate ignoring saved page {}", from.page);
            }
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
            // The storage rail is already up, or the card would not be mounted.
            // The codec's output stage is a different matter: `play <16 hex>`
            // leaves raising it to whoever typed it, and nobody typed this.
            OUTPUT_REQUEST.store(OUTPUT_UP, Ordering::Relaxed);
            REQUEST.store(request, Ordering::Relaxed);
        }

        // Pausing and stopping are the same act here. Nothing remembers where a
        // story got to, so a pause that could be resumed does not yet exist.
        Action::Pause | Action::Stop => {
            esp_println::println!("teddiebox: plate stopping");
            audio::STOP.store(true, Ordering::Relaxed);
        }

        Action::AbortFetch => {
            esp_println::println!("teddiebox: plate abandoning the download");
            DOWNLOAD_ABORT.store(true, Ordering::Relaxed);
        }

        Action::RequestContent(tag) => {
            let ruid = ruid_of(tag);
            esp_println::println!("teddiebox: plate fetching {ruid:016X}");
            // Written whole, in the one call: see `FetchRequest` for why the
            // ruid and the token it authorises never travel separately.
            critical_section::with(|cs| {
                *FETCH_REQUEST.borrow_ref_mut(cs) = Some(FetchRequest { ruid, token });
            });
            NET_REQUEST.store(NET_GET, Ordering::Relaxed);
        }

        Action::PlayPrompt(prompt) => match Sound::for_prompt(prompt) {
            Some(sound) => SOUND_REQUEST.store(sound.file(), Ordering::Relaxed),
            // No file on this card has been identified for this sentence, and
            // substituting another would have the box say something true about
            // the wrong thing. The console hears it; the child hears nothing.
            None => esp_println::println!(
                "teddiebox: plate no sound identified for {prompt:?} — saying nothing"
            ),
        },

        // Position memory has no store yet: `saved_position` answers zero for
        // every figure, so there is nothing here for this to be written to.
        Action::SavePosition { .. } => {}

        // `SetLed` is reachable and the rest are not, and neither is acted on.
        // The LED belongs to the console loop and has no seam another task can
        // reach through; saying what was wanted is the honest half of that.
        other => esp_println::println!("teddiebox: plate not wired — {other:?}"),
    }
}

/// Reads what is on the plate right now, if it has changed since the last
/// read, and says what happened in the reducer's words.
///
/// The figure and the token it arrived with are updated together, here and
/// nowhere else. They are two halves of one identity, and letting them drift
/// apart is the bug [`FetchRequest`] exists to make unrepresentable.
///
/// Called from two places — once per pass of the media loop, and once per
/// frame while a story plays — because a loop pass is a whole story long and a
/// figure lifted during one must not wait for it to end.
fn take_plate_event(on_plate: &mut Option<TagUid>, token: &mut Option<[u8; 32]>) -> Option<Event> {
    Some(match PLATE_TAG.try_take()? {
        PlateState::Present { uid, token: read } => {
            let tag = TagUid(uid);
            *on_plate = Some(tag);
            *token = read;
            Event::TagPresent(tag)
        }
        PlateState::Absent => {
            *on_plate = None;
            *token = None;
            Event::TagAbsent
        }
    })
}

/// Tells the reducer what happened and carries out what it decides.
///
/// The index is built here and dropped at the end: it borrows the card, and
/// the rest of the media loop needs the card unborrowed — which is the whole
/// reason it is this cheap to construct.
fn apply(reducer: &mut Core, card: &storage::Mounted, event: Event, token: Option<[u8; 32]>) {
    let index = CardIndex::new(card);
    for action in reducer.handle(event, &index) {
        perform(action, &index, token);
    }
}

#[embassy_executor::task]
async fn media(
    spi: Spi<'static, esp_hal::Blocking>,
    cs: Output<'static>,
    i2s_tx: esp_hal::i2s::master::I2sTx<'static, esp_hal::Blocking>,
    mut tone_buffer: esp_hal::dma::DmaLoopBuf,
    wav_buffer: esp_hal::dma::DmaTxStreamBuf,
) {
    // Exactly one cycle of a sine, looped by the DMA forever.
    //
    // A stream buffer was tried first and underran: the transfer starts as
    // soon as it is created, ran off the end of an empty buffer before the
    // first sample was pushed, and never restarted. A fixed repeating waveform
    // wants a buffer the DMA repeats rather than one the CPU refills.
    for (i, sample) in tone::SINE.iter().enumerate() {
        let bytes = sample.to_le_bytes();
        // Interleaved stereo, the same sample in both slots. The codec takes
        // its speaker path from the left channel and its headphones from both,
        // so a single channel would make the result depend on which output is
        // being listened to.
        tone_buffer[i * 4] = bytes[0];
        tone_buffer[i * 4 + 1] = bytes[1];
        tone_buffer[i * 4 + 2] = bytes[0];
        tone_buffer[i * 4 + 3] = bytes[1];
    }

    // Every one of these is claimed once and not returned, so each is held in
    // an Option and taken when its command arrives.
    let mut card: Option<storage::Mounted> = None;
    // The card is read for configuration once. A second read would undo a
    // bench override typed since, which is the opposite of what the console
    // commands are for.
    let mut configured = false;
    let mut bus = Some((spi, cs));
    let mut i2s_tx = Some(i2s_tx);
    let mut tone_buffer = Some(tone_buffer);
    let mut wav_buffer = Some(wav_buffer);
    // The tone's transfer stops when it is dropped, so it is parked here for
    // as long as the tone should play — which is until the box restarts.
    let mut _tone_transfer = None;

    let mut download: Option<CacheWrite> = None;

    // The thing that decides what a figure on the plate means. It lives here
    // because the only question it asks — what is on the card — can only be
    // answered by the task that owns the card.
    let mut reducer = Core::new(CoreConfig::default());
    // Which figure the box is currently answering for, or none. A download
    // ends elsewhere and says only that it ended; this is what turns that into
    // an event about a particular story. `None` is also the test for "nobody
    // is waiting any more", which is what makes a fetch that finishes after
    // the figure was lifted harmless.
    let mut on_plate: Option<TagUid> = None;
    // The token that came with the figure now on the plate. Kept here rather
    // than in a shared static: it is read exactly once, when the reducer
    // actually asks for a fetch, and bundled with that figure's ruid into one
    // `FetchRequest` write — so nothing typed at the console between now and
    // then can attach itself to this figure's fetch.
    let mut on_plate_token: Option<[u8; 32]> = None;

    loop {
        // Only four of the ten events are fed in this increment. Ears, motion
        // and above all `Tick` are deliberately withheld: `Core` emits
        // `PowerOff` on a tick once the idle timeout passes, which on a bench
        // box sitting at a console would switch it off mid-session. Wiring the
        // rest is its own piece of work with its own uncalibrated constants.
        //
        // Fed before the request is read, so a `Play` decided here is picked
        // up on the same pass rather than the next one.
        let mut events: [Option<Event>; 2] = [None, None];

        events[0] = take_plate_event(&mut on_plate, &mut on_plate_token);

        // A download has ended somewhere this task cannot see. Read every pass
        // so a stale answer cannot arrive later attached to a different figure.
        let outcome = FETCH_OUTCOME.swap(FETCH_NOTHING, Ordering::Relaxed);
        if outcome != FETCH_NOTHING {
            let outcome_ruid = FETCH_OUTCOME_RUID.load(Ordering::Relaxed);
            match on_plate {
                // The figure this outcome was for is still on the plate: the
                // reducer's guard checks the same identity again before
                // acting, but the event it sees is at least about the right
                // figure.
                Some(tag) if ruid_of(tag) == outcome_ruid => {
                    events[1] = match outcome {
                        FETCH_COMPLETED => Some(Event::ContentReady(tag)),
                        FETCH_UNREACHABLE => {
                            Some(Event::ContentMissing(tag, Unavailable::Unreachable))
                        }
                        FETCH_NO_CONTENT => {
                            Some(Event::ContentMissing(tag, Unavailable::NoContent))
                        }
                        _ => unreachable!("outcome != FETCH_NOTHING was just checked"),
                    };
                }
                // A figure is on the plate, but this outcome belongs to a
                // different one — a console `get` that finished while
                // something else sits here. Attributing it to the figure on
                // the plate is exactly the bug this identity exists to
                // prevent, so the reducer never sees it.
                Some(_) => esp_println::println!(
                    "teddiebox: plate ignoring a fetch outcome for {outcome_ruid:016X} — \
                     not the figure on the plate"
                ),
                // Nobody is waiting. Already the harmless case this read
                // exists to guarantee.
                None => {}
            }
        }

        if events.iter().any(Option::is_some) {
            match card.as_ref() {
                Some(mounted) => {
                    for event in events.into_iter().flatten() {
                        apply(&mut reducer, mounted, event, on_plate_token);
                    }
                }
                // A figure answered with nothing at all is the failure this
                // whole feature exists to remove, so it is at least said out
                // loud on the one channel that is left.
                None => esp_println::println!(
                    "teddiebox: plate no card mounted — the figure cannot be answered"
                ),
            }
        }

        let request = REQUEST.swap(REQUEST_NONE, Ordering::Relaxed);
        if request == REQUEST_NONE {
            // Serviced here rather than in a branch of its own, so a download
            // makes progress in the gaps without ever owning the loop.
            let downloading = service_download(card.as_ref(), &mut download);
            // A late drain stalls the socket, so poll hard while bytes are
            // moving and idle politely when they are not.
            let gap = if downloading { 5 } else { 100 };
            Timer::after(Duration::from_millis(gap)).await;
            continue;
        }

        // The console loop raised the storage rail before setting the request.
        // Devices need their supply settled before they answer — a scan against
        // an unsettled rail is what invented an I2C device at 0x09 in step 4.
        if matches!(
            request,
            REQUEST_WALK
                | REQUEST_WAV
                | REQUEST_TAF
                | REQUEST_CONTENT
                | REQUEST_CACHE
                | REQUEST_PCM
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
                    card = Some(mounted)
                }
                Err(reason) => {
                    // The bus was consumed by the attempt, so nothing needing
                    // the card can be retried without a restart.
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

            REQUEST_WAV | REQUEST_TAF | REQUEST_CONTENT | REQUEST_CACHE => {
                let Some(card) = card.as_ref() else {
                    continue;
                };
                // Checked before anything is taken. Building a tuple of takes
                // and matching on it afterwards drops whichever resource did
                // come back when the other did not — which quietly destroyed
                // the DMA buffer a later command still needed.
                if i2s_tx.is_none() || wav_buffer.is_none() {
                    esp_println::println!(
                        "teddiebox: the audio hardware is already claimed — rb to run another"
                    );
                    continue;
                }
                let (Some(tx), Some(buffer)) = (i2s_tx.take(), wav_buffer.take()) else {
                    continue;
                };

                // A stop typed before the first frame must not be waiting for
                // the next playback to start.
                audio::STOP.store(false, Ordering::Relaxed);

                // Raised around every path that feeds the DMA, so the
                // download knows to leave the radio alone. Cleared on the way
                // out whatever happened — a download paused for ever because
                // playback failed would be a worse bug than the glitch this
                // avoids.
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
                    // Answered on every frame, because a story is one pass of
                    // this loop and can be half an hour long. Without it the
                    // plate is not read again until the story ends by itself,
                    // and lifting the figure does nothing — the reducer never
                    // sees the departure, so it never asks for the stop.
                    let mut watch_plate = || {
                        if let Some(event) = take_plate_event(&mut on_plate, &mut on_plate_token) {
                            apply(&mut reducer, card, event, on_plate_token);
                        }
                    };
                    // The hardware comes back, so playing again needs no
                    // reboot — which is what makes stopping worth anything.
                    let (outcome, tx, buffer) =
                        audio::play_taf(card, tx, buffer, source, &mut watch_plate).await;
                    if let Err(reason) = outcome {
                        esp_println::println!("teddiebox: playback failed — {reason}");
                    }
                    ANNOUNCING.store(false, Ordering::Relaxed);
                    i2s_tx = Some(tx);
                    wav_buffer = Some(buffer);
                    // Nothing is playing now, so the speaker has no business
                    // being driven — unless something has already asked for
                    // the next thing to play, in which case it raised the
                    // output when it asked and lowering it here would silence
                    // it. It also spares the stage a power cycle it would
                    // otherwise click through.
                    if REQUEST.load(Ordering::Relaxed) == REQUEST_NONE {
                        OUTPUT_REQUEST.store(OUTPUT_DOWN, Ordering::Relaxed);
                    }
                }
                PLAYING.store(false, Ordering::Relaxed);
            }

            _ => {}
        }
    }
}

/// Brings the NFC reader up on first use and answers the bench commands.
///
/// Waits for a request rather than starting at boot: the reader shares the
/// Associates, takes a lease, and stays up until asked down.
///
/// The whole association lives inside one call because `acquire` lends the
/// session and the link out of the radio: they cannot outlive it, and the link
/// has to be polled for the entire time the session is wanted. So "up" is not
/// a state this returns to the caller — it is a stretch of time spent inside
/// here, ending only when the console asks for it to end.
async fn bring_up(radio: &mut net::Radio<'_>, tls: Option<mbedtls_rs::TlsReference<'_>>) {
    let Some(config) = credentials() else {
        esp_println::println!(
            "teddiebox: net no credentials — type `net ssid <name>` then `net pw <passphrase>`"
        );
        return;
    };

    // The stack seeds its port and transaction numbers from this, so it has to
    // differ between boots. The moment somebody typed `net up` is as good a
    // source as this firmware has and better than a constant.
    let seed = Instant::now().as_micros();

    let (mut session, mut link) = match radio.acquire(&config, seed) {
        Ok(pair) => pair,
        Err(e) => {
            esp_println::println!("teddiebox: net could not power the radio — {e:?}");
            return;
        }
    };
    esp_println::println!("teddiebox: net associating with {}", config.ssid);

    // The runner has to be polled throughout, not awaited first: it never
    // returns, and nothing else here makes progress without it.
    match select(link.run(), session.connect()).await {
        Either::First(_) => unreachable!("the runner never returns"),
        Either::Second(Err(e)) => {
            esp_println::println!("teddiebox: net association refused — {e:?}");
            net::release(session, link);
            return;
        }
        Either::Second(Ok(())) => {}
    }
    esp_println::println!("teddiebox: net associated — asking for an address");

    let stack = session.stack();
    // Associated and addressed are different things and fail for different
    // reasons; a box that joins the network and never gets a lease should say
    // so rather than report success.
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
            return;
        }
        Either::Second(Either::First(())) => report_address(&stack),
    }

    // Up. Stay here, polling the stack, until the console says otherwise —
    // dropping the runner between requests would stall the very thing that
    // keeps the lease alive.
    let until_down = async {
        loop {
            match NET_REQUEST.swap(REQUEST_NONE, Ordering::Relaxed) {
                NET_DOWN => break,
                NET_STATUS => report_address(&stack),
                // The probe runs here rather than in the outer task because it
                // needs the stack, and the stack only exists between
                // `acquire` and `release`. It is awaited inline, so the runner
                // beside it keeps polling for its whole length — a handshake
                // is several round trips and would stall without it.
                NET_GET => match tls {
                    None => esp_println::println!("teddiebox: tls context unavailable"),
                    Some(tls) => match critical_section::with(|cs| *FETCH_REQUEST.borrow_ref(cs)) {
                        None => esp_println::println!(
                            "teddiebox: get has no request queued — nothing to fetch"
                        ),
                        Some(FetchRequest {
                            ruid: requested,
                            token,
                        }) => {
                            // Attributed up front, from the pair this fetch was
                            // raised with — see `FetchRequest` — rather than read
                            // from a shared static later, when a `token` typed at
                            // the console in the meantime could have moved on.
                            FETCH_ACTIVE_RUID.store(requested, Ordering::Relaxed);
                            let ruid = requested.to_be_bytes();
                            // `content_path` takes the UID and reverses it itself,
                            // and what the console typed is already reversed.
                            let mut uid = ruid;
                            uid.reverse();
                            let path = teddiebox_download::content_path(uid);

                            DOWNLOAD_DIR.store(path.directory, Ordering::Relaxed);
                            DOWNLOAD_FILE.store(path.file, Ordering::Relaxed);
                            DOWNLOAD_SENT.store(0, Ordering::Relaxed);
                            critical_section::with(|cs| {
                                *DOWNLOAD_PIPE.borrow_ref_mut(cs) = Pipe::new()
                            });
                            // The card is the only thing that knows what is
                            // already downloaded, and only its owner may ask it. So
                            // the request waits here until it has been told what to
                            // ask for — a resume asks for the rest, and asking for
                            // the whole file again is a quarter of an hour thrown
                            // away with the story still not playing.
                            DOWNLOAD_FROM.store(0, Ordering::Relaxed);
                            DOWNLOAD_AT.store(0, Ordering::Relaxed);
                            DOWNLOAD_STATE.store(DOWNLOAD_PREPARING, Ordering::Relaxed);
                            // Bounded, because a media task that never answers must
                            // not wedge the console. Timing out leaves DOWNLOAD_FROM
                            // at zero, which fetches the whole file — slow, and
                            // correct, which is the right way round for a fallback.
                            let mut waited = 0;
                            while DOWNLOAD_STATE.load(Ordering::Relaxed) == DOWNLOAD_PREPARING
                                && waited < 500
                            {
                                Timer::after(Duration::from_millis(10)).await;
                                waited += 1;
                            }

                            let from = DOWNLOAD_FROM.load(Ordering::Relaxed);
                            if from == NOTHING_TO_FETCH {
                                esp_println::println!(
                                    "teddiebox: get the card already holds all of it"
                                );
                                // Nothing to fetch is still an answer, and it is
                                // the good one: whoever asked can play now.
                                fetch_ended(FETCH_COMPLETED);
                                DOWNLOAD_STATE.store(DOWNLOAD_IDLE, Ordering::Relaxed);
                            } else {
                                if from > 0 {
                                    esp_println::println!("teddiebox: get resuming from {from}");
                                }
                                DOWNLOAD_ABORT.store(false, Ordering::Relaxed);
                                if token.is_some() {
                                    esp_println::println!("teddiebox: get sending the tag's token");
                                }
                                let mut throttle = Throttle::new();
                                let mut may_fetch = || {
                                    throttle.update(
                                        PLAYING.load(Ordering::Relaxed),
                                        NOTHING_WAITING,
                                        RESUME_BELOW,
                                        PAUSE_ABOVE,
                                    )
                                };
                                let mut into_pipe = |bytes: &[u8]| -> usize {
                                    // Nobody is draining. Swallow the rest so the
                                    // download ends and reports, rather than blocking
                                    // forever on a pipe that will never empty.
                                    if DOWNLOAD_ABORT.load(Ordering::Relaxed) {
                                        return bytes.len();
                                    }
                                    let taken = critical_section::with(|cs| {
                                        DOWNLOAD_PIPE.borrow_ref_mut(cs).write(bytes)
                                    });
                                    DOWNLOAD_SENT.fetch_add(taken as u32, Ordering::Relaxed);
                                    taken
                                };
                                let resume_etag = critical_section::with(|cs| {
                                    DOWNLOAD_RESUME_ETAG.borrow_ref(cs).clone()
                                });
                                let wanted = tls::Wanted {
                                    server: &config.server,
                                    insecure: config.insecure,
                                    ruid,
                                    token: token.as_ref(),
                                    from: (from > 0).then_some(from),
                                    etag: resume_etag.as_ref(),
                                };
                                // Raised here rather than before the request. Until the
                                // head arrives there is nothing to write and nothing to
                                // write it with — and opening the file any earlier
                                // truncates a partial download on behalf of a request
                                // that may yet fail before asking for a byte.
                                let mut on_head = |head: tls::Head<'_>| {
                                    DOWNLOAD_TOTAL
                                        .store(head.total.unwrap_or(0), Ordering::Relaxed);
                                    // Where the body actually belongs, which is not
                                    // always where it was asked to start: a server
                                    // that declined the range says so with zero.
                                    DOWNLOAD_AT.store(head.offset, Ordering::Relaxed);
                                    critical_section::with(|cs| {
                                        *DOWNLOAD_ETAG.borrow_ref_mut(cs) = head.etag.cloned();
                                    });
                                    DOWNLOAD_STATE.store(DOWNLOAD_RUNNING, Ordering::Relaxed);
                                };
                                let outcome = tls::fetch(
                                    tls,
                                    &stack,
                                    &wanted,
                                    &mut on_head,
                                    &mut into_pipe,
                                    &mut may_fetch,
                                )
                                .await;
                                // Ended either way: the consumer must stop waiting for
                                // bytes that are never coming.
                                DOWNLOAD_STATE.store(DOWNLOAD_ENDED, Ordering::Relaxed);

                                match outcome {
                                    // Not reported ready here. The bytes are still
                                    // travelling through the pipe to the card, and
                                    // the media task says so when they land.
                                    Ok(got) => esp_println::println!(
                                        "teddiebox: get received {} bytes, crc32 {:08X}, {} s",
                                        got.bytes,
                                        got.crc32,
                                        got.seconds
                                    ),
                                    Err(e) => {
                                        // The console keeps the real code; the
                                        // reducer gets the one word it can act on.
                                        esp_println::println!("teddiebox: get failed — {e:?}");
                                        fetch_ended(match why_unavailable(&e) {
                                            Unavailable::NoContent => FETCH_NO_CONTENT,
                                            Unavailable::Unreachable => FETCH_UNREACHABLE,
                                        });
                                    }
                                }
                            }
                        }
                    },
                },
                NET_TLS => match tls {
                    None => esp_println::println!(
                        "teddiebox: tls context unavailable — mbedtls would not start"
                    ),
                    Some(tls) => {
                        match tls::probe(tls, &stack, &config.server, config.insecure).await {
                            Ok(()) => esp_println::println!("teddiebox: tls ok"),
                            Err(e) => esp_println::println!("teddiebox: tls failed — {e:?}"),
                        }
                    }
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
/// The radio has never run on this board, so this exists before anything that
/// associates: a scan needs no credentials, no card and no network stack, and
/// so separates "the radio does not work" from "the password is wrong" once
/// and for all.
#[embassy_executor::task]
async fn net(
    wifi: esp_hal::peripherals::WIFI<'static>,
    sha: esp_hal::peripherals::SHA<'static>,
    rsa: esp_hal::peripherals::RSA<'static>,
    aes: esp_hal::peripherals::AES<'static>,
) {
    let mut radio = net::Radio::new(wifi);
    // Built once, before anything associates. mbedtls keeps global state, and
    // the statics behind this can only be filled once — so a failure here is
    // permanent for this boot rather than something to retry per connection.
    // The three crypto peripherals come along because the hooks that route
    // mbedtls onto them have to be registered before this call builds the
    // first mbedtls context; see `tls::init`.
    let tls = tls::init(sha, rsa, aes);
    if tls.is_none() {
        esp_println::println!("teddiebox: tls could not be initialised");
    }

    loop {
        // One read of the request, dispatched once. Two reads would race: the
        // first would consume a request the second was meant to handle, and
        // `net up` would be swallowed by the scan's test and silently lost.
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
                        // Hitting the quota means the driver stopped collecting,
                        // not that the band holds exactly this many — and what it
                        // dropped is whatever sits on the highest channels.
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
                    // Nothing came back at all. The driver raises `ScanDone` even
                    // for an empty scan, so silence points at the radio never
                    // having started rather than at an empty band.
                    Either::Second(()) => esp_println::println!(
                        "teddiebox: net scan timed out after {} s — the radio never answered",
                        SCAN_TIMEOUT.as_secs()
                    ),
                }
            }
            NET_UP => bring_up(&mut radio, tls).await,
            // `net status` and `net down` are answered inside `bring_up` while
            // it is running. Reaching them here means it is not.
            NET_STATUS | NET_DOWN | NET_TLS => {
                esp_println::println!("teddiebox: net is down")
            }
            // Same message, but somebody is waiting on this one: a figure whose
            // story has to be fetched with the radio down is unreachable, and
            // saying nothing would leave the box silent with no explanation.
            NET_GET => match critical_section::with(|cs| *FETCH_REQUEST.borrow_ref(cs)) {
                None => {
                    esp_println::println!("teddiebox: get has no request queued — nothing to fetch")
                }
                Some(FetchRequest { ruid, .. }) => {
                    // Attributed before the outcome is recorded, the same as
                    // every other path that ends a fetch — this one is no
                    // exception just because the radio never came up.
                    FETCH_ACTIVE_RUID.store(ruid, Ordering::Relaxed);
                    esp_println::println!("teddiebox: net is down");
                    fetch_ended(FETCH_UNREACHABLE);
                }
            },
            _ => {}
        }
        Timer::after(Duration::from_millis(100)).await;
    }
}

/// storage rail, and raising that rail is the console loop's decision.
#[embassy_executor::task]
async fn nfc_reader(
    spi: Spi<'static, esp_hal::Blocking>,
    cs: Output<'static>,
    irq: Input<'static>,
) {
    while NFC_REQUEST.load(Ordering::Relaxed) == REQUEST_NONE
        && !PLATE_POLLING.load(Ordering::Relaxed)
    {
        Timer::after(Duration::from_millis(100)).await;
    }

    // The rail was raised by the console loop; give it the same settling time
    // the card and the I2C devices get.
    Timer::after(Duration::from_millis(50)).await;

    let mut reader = match nfc::Reader::open(spi, cs, irq, esp_hal::delay::Delay::new()) {
        Ok(reader) => reader,
        Err(reason) => {
            esp_println::println!("teddiebox: nfc failed — {reason}");
            return;
        }
    };

    let mut presence = Presence::new(ARRIVALS_TO_AGREE, MISSES_TO_LEAVE);
    let mut ticks_since_poll: u8 = 0;
    // Whether the last poll found a figure. Chooses which question the next
    // poll asks first; a wrong guess costs one extra unanswered exchange and
    // corrects itself on the following poll, which `MISSES_TO_LEAVE` absorbs.
    let mut believed_present = false;

    loop {
        match NFC_REQUEST.swap(REQUEST_NONE, Ordering::Relaxed) {
            NFC_INVENTORY => {
                reader.inventory();

                // Is the antenna even connected? With our own field off, the
                // RSSI register reports RF arriving from outside, so an
                // external source proves the coil is coupled to the chip.
                // Nothing else here can tell a disconnected antenna from an
                // empty plate.
                esp_println::println!(
                    "teddiebox: nfc listening for an external field for 6 s — \
                     hold an NFC phone against the plate"
                );
                reader.set_field(false);
                let mut peak = 0u8;
                for _ in 0..60 {
                    peak = peak.max(reader.rssi());
                    Timer::after(Duration::from_millis(100)).await;
                }
                reader.set_field(true);
                if peak == 0 {
                    esp_println::println!(
                        "teddiebox: nfc heard nothing at all — the antenna is not coupled"
                    );
                } else {
                    esp_println::println!(
                        "teddiebox: nfc external field peaked at {peak:#04x} — the antenna works"
                    );
                }
            }
            NFC_UNLOCK => {
                let password = NFC_PASSWORD.load(Ordering::Relaxed);
                if password == 0 {
                    esp_println::println!("teddiebox: nfc no password set — type `pw <8 hex>`");
                } else {
                    reader.unlock(password);
                }
            }
            NFC_FORCE_UNLOCK => {
                let password = NFC_PASSWORD.load(Ordering::Relaxed);
                if password == 0 {
                    esp_println::println!("teddiebox: nfc no password set — type `pw <8 hex>`");
                } else {
                    reader.force_unlock(password);
                }
            }
            NFC_LOCK => {
                let password = NFC_PASSWORD.load(Ordering::Relaxed);
                if password == 0 {
                    esp_println::println!("teddiebox: nfc no password set — type `pw <8 hex>`");
                } else {
                    reader.lock(password);
                }
            }
            NFC_READ_MEMORY => {
                let range = NFC_MEM_RANGE.load(Ordering::Relaxed);
                reader.dump_memory((range >> 8) as u8, range as u8);
            }
            NFC_READ_TOKEN => match reader.read_token() {
                Some(token) => {
                    critical_section::with(|cs| *TAG_TOKEN.borrow_ref_mut(cs) = Some(token));
                    // The length, never the value.
                    esp_println::println!(
                        "teddiebox: nfc token read, {} bytes — `get` will send it",
                        token.len()
                    );
                }
                None => esp_println::println!(
                    "teddiebox: nfc token unreadable — unlock first with `pw` then `slix`"
                ),
            },
            _ => {}
        }

        // Read out when polling stops, so a whole run is summarised by its
        // worst reply rather than by whichever poll happened to print last.
        if PLATE_REPORT.swap(false, Ordering::Relaxed) {
            let (polls, micros) = reader.slowest_reply();
            esp_println::println!(
                "teddiebox: plate slowest reply {polls} polls (~{micros} us) \
                 of {} attempts allowed",
                trf7962a::IRQ_POLL_ATTEMPTS
            );
        }

        if PLATE_POLLING.load(Ordering::Relaxed) {
            ticks_since_poll = ticks_since_poll.saturating_add(1);
            if ticks_since_poll >= PLATE_POLL_TICKS {
                ticks_since_poll = 0;

                let password = NFC_PASSWORD.load(Ordering::Relaxed);

                // Which question is cheap depends on what was there last time,
                // and the difference is not small: measured on 2026-09-07, a
                // poller that always asked the full question cost 49 DMA
                // restarts in 70 s of playback against 0 with polling off,
                // because every unanswered exchange blocks this task for the
                // whole `IRQ_POLL_ATTEMPTS` window.
                let seen = if believed_present {
                    // A figure identified once stays out of privacy mode until
                    // its field is cycled, so a plain inventory answers on the
                    // first try. It also re-reads the UID, which is what
                    // notices one figure being swapped for another.
                    reader.identify()
                } else if reader.tag_present() {
                    // Something is there but has not been identified yet. This
                    // is the only poll that pays for the password exchange.
                    reader.inventory_unlocked(password)
                } else {
                    // The state the box sits in almost all the time: one
                    // unanswered exchange and nothing else.
                    None
                };
                believed_present = seen.is_some();

                if let Some(event) = presence.feed(seen.map(TagUid)) {
                    match event {
                        TagEvent::Arrived(TagUid(uid)) => {
                            // Read the token in the same session that found
                            // the tag: it is only readable while the figure
                            // is on the plate and unlocked, and it is needed
                            // when teddyCloud has to go upstream for the
                            // story.
                            let token = reader.read_token();
                            PLATE_TAG.signal(PlateState::Present { uid, token });
                        }
                        TagEvent::Left => PLATE_TAG.signal(PlateState::Absent),
                    }
                }
            }
        } else {
            ticks_since_poll = 0;
        }

        Timer::after(Duration::from_millis(100)).await;
    }
}

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    let p = esp_hal::init(esp_hal::Config::default());

    // esp-radio allocates. The rest of this firmware does not, and libopus in
    // particular must not — its hardening path is the only thing that ever
    // reaches libc, and it panics rather than allocating. This heap belongs to
    // the radio stack alone.
    //
    // It is taken out of the main task's stack, not out of spare memory.
    // `esp-hal`'s linker script puts `.stack` at the top of DRAM running down
    // to wherever `.bss` ends, so the stack is whatever the static data leaves
    // behind: adding this heap moved `_stack_start - _stack_end` from 219_788
    // bytes to 137_104. That is the number a bench measures, not the heap
    // size, because it is the one that decides whether the box still boots.
    // Before anything has had a chance to go deep. Everything below this
    // frame is free right now, and whatever is used before this point is
    // invisible to the measurement afterwards.
    //
    // Bracketed by prints because the first version of this trapped on the
    // stack canary and the box came back **silent** — no console, no panic,
    // nothing to say which line did it, and a J100 recovery to undo. A pair of
    // lines costs nothing and turns that into a bisect of one.
    esp_println::println!("teddiebox: painting the stack");
    stack::paint();
    esp_println::println!("teddiebox: stack painted");

    esp_alloc::heap_allocator!(size: RADIO_HEAP);

    let timg0 = TimerGroup::new(p.TIMG0);
    esp_rtos::start(timg0.timer0, p.FROM_CPU_INTR0);

    // Clear the ROM's force-download-boot request. It lives in the RTC domain
    // and survives a reset — that is what makes `dl` work — so leaving it set
    // would send every future reset back into download mode. Clearing it here
    // means the box can only ever be one reset away from running again.
    esp_hal::peripherals::LPWR::regs()
        .option1()
        .modify(|_, w| w.force_download_boot().clear_bit());

    let mut board = BoardPins::new(p.GPIO45, p.GPIO47);
    let mut gates = Gates::at_reset();

    spawner.spawn(heartbeat().unwrap());
    spawner.spawn(net(p.WIFI, p.SHA, p.RSA, p.AES).unwrap());

    // Ears and wake are all active low, so they are read with a pull-up: an
    // unconnected input then reads as "not pressed" rather than floating into
    // phantom presses.
    let up = InputConfig::default().with_pull(Pull::Up);
    spawner.spawn(
        inputs(
            Input::new(p.GPIO20, up),
            Input::new(p.GPIO21, up),
            Input::new(p.GPIO7, up),
        )
        .unwrap(),
    );

    let mut adc_config = AdcConfig::new();
    let battery = adc_config.enable_pin(p.GPIO9, Attenuation::_11dB);
    let charger = adc_config.enable_pin(p.GPIO8, Attenuation::_11dB);
    spawner.spawn(sense(Adc::new(p.ADC1, adc_config), battery, charger).unwrap());

    // The LED is on this rail, so it has to come up first.
    board.apply(gates.power(Rail::Peripherals, true));

    // UART0's receive half. esp-println keeps the transmit half.
    let mut console = UartRx::new(p.UART0, UartConfig::default().with_baudrate(115200))
        .expect("UART0 receive")
        .with_rx(p.GPIO44);
    let mut watch = CommandWatch::new();
    // Cleared the moment it is asked for, so a jingle is a start-up event and
    // not something that can happen twice.
    let mut startup_pending = true;
    esp_println::println!(
        "teddiebox: dl rb | t wav taf play <id>[/<id>|<16hex>] stop (loud) | sd | nfc pw slix slixp lock mem <2hex> <2hex> token | net scan ssid <name> pw <pass> insecure yes|no up down tls status | get <16hex> | stack | cinit cdown cset cclr out spk | pcm <2hex>"
    );

    // Audio out on I2S: DIN 10, BCLK 11, WCLK 12, at the rate the codec's PLL
    // was configured for. The SD card is SPI2 on CLK 35, MOSI 38, MISO 36 with
    // CS 34, created at the specification's 400 kHz initialisation rate;
    // storage.rs raises it once the card has identified itself.
    //
    // Both go to one task: the tone, the checksum walk and WAV playback all
    // contend for these two peripherals.
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
            // Idle high: on SPI a card watches for its select line to fall, and
            // one that starts low is addressed before anything is ready to talk
            // to it.
            let cs = Output::new(p.GPIO34, Level::High, OutputConfig::default());

            let tone_buffer = esp_hal::dma_loop_buffer!(tone::SINE.len() * 4);
            // Roughly 170 ms of audio at 48 kHz stereo 16-bit. Design §5 wants
            // the cushion sized from measurement rather than estimate, and this
            // is the buffer whose low-water mark provides that measurement.
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
                    spawner.spawn(nfc_reader(nfc_spi, nfc_cs, nfc_irq).unwrap());
                }
                Err(_) => esp_println::println!("teddiebox: NFC SPI would not configure"),
            }
        }
        (Err(_), _) => esp_println::println!("teddiebox: I2S would not configure"),
        (_, Err(_)) => esp_println::println!("teddiebox: SPI would not configure"),
    }

    // Devices need a moment after their rail comes up before they answer.
    // Without this the accelerometer misses the scan and then answers the
    // probe a few milliseconds later, which reads as a bus that is lying.
    Timer::after(Duration::from_millis(50)).await;

    // The codec and the accelerometer are both on the rail brought up above,
    // so the bus is only worth scanning now.
    match I2c::new(p.I2C0, I2cConfig::default()) {
        Ok(i2c) => {
            let mut i2c = i2c.with_sda(p.GPIO5).with_scl(p.GPIO6);
            scan_i2c(&mut i2c);
            let reset = Output::new(p.GPIO26, Level::Low, OutputConfig::default());
            spawner.spawn(motion(i2c, reset).unwrap());
        }
        Err(_) => esp_println::println!("teddiebox: I2C would not configure"),
    }

    // A dim green breath. The waveform is held by the LEDC peripheral, so
    // this loop only decides how bright and then goes back to sleep — it does
    // not have to be on time. The software PWM this replaces woke four hundred
    // times a second and could not keep its own period once anything else
    // wanted the executor.
    const BREATH_STEP: Duration = Duration::from_millis(20);

    // The controller and its timer are bound here rather than inside the
    // helper because the channels borrow the timer, so it has to outlive them.
    // `main` never returns, which is exactly long enough.
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

    let lit = gates.led(Colour::Green).expect("the rail is up");

    loop {
        if let Some(rgb) = rgb.as_ref() {
            rgb.apply(
                &lit,
                board::breathing_duty(Instant::now().as_millis() as u32),
            );
        }

        // The jingle stock plays at power-on, once the codec could carry it.
        //
        // It is not decoration. Powering the class-D amplifier is audible on
        // its own — that is what this box used to click on — and starting the
        // audio in the same breath is how the stock firmware lives with it.
        // The sound covers the transient rather than the transient being
        // removed, which is the same trick, honestly arrived at.
        if startup_pending && CODEC_READY.load(Ordering::Relaxed) {
            startup_pending = false;
            SOUND_REQUEST.store(Sound::Startup.file(), Ordering::Relaxed);
        }

        // A sound the box decided to say about itself. Raised here rather
        // than in the task that noticed, because the rails and the playback
        // request belong to this loop.
        let pending = SOUND_REQUEST.swap(NO_SOUND, Ordering::Relaxed);
        if pending != NO_SOUND {
            board.apply(gates.power(Rail::Storage, true));
            OUTPUT_REQUEST.store(OUTPUT_UP, Ordering::Relaxed);
            CONTENT_DIRECTORY.store(LANGUAGE.content_directory(), Ordering::Relaxed);
            CONTENT_FILE.store(pending, Ordering::Relaxed);
            // Marked here rather than by the task that plays it, so there is
            // no window in which the announcement is pending but nothing
            // reports it as under way.
            ANNOUNCING.store(true, Ordering::Relaxed);
            REQUEST.store(REQUEST_CONTENT, Ordering::Relaxed);
        }

        // The box said it was turning off. Do it, once the sentence has been
        // heard — cutting the announcement short to obey it would be its own
        // kind of broken.
        if SHUTTING_DOWN.load(Ordering::Relaxed) && !ANNOUNCING.load(Ordering::Relaxed) {
            esp_println::println!("teddiebox: battery critical — going dark");
            quieten_codec().await;
            board.apply_all(&gates.release_for_reset());
            drain_console();
            // Everything a child can see or hear is now off. This is not a
            // true power-off — the chip is still running, and deep sleep is
            // bench step 12 — so it is parked here rather than pretending
            // otherwise.
            loop {
                Timer::after(Duration::from_secs(60)).await;
            }
        }

        let mut buf = [0u8; 16];
        if let Ok(n) = console.read_buffered(&mut buf) {
            match buf[..n].iter().find_map(|&b| watch.feed(b)) {
                Some(Command::DownloadMode) => {
                    quieten_codec().await;
                    reboot_to_download(&mut board, &mut gates)
                }
                Some(Command::Reboot) => {
                    quieten_codec().await;
                    reboot(&mut board, &mut gates)
                }
                Some(Command::Tone) => {
                    // The output path is unpowered until something plays, so
                    // every audio command has to ask for it first or it is
                    // heard by nobody.
                    OUTPUT_REQUEST.store(OUTPUT_UP, Ordering::Relaxed);
                    REQUEST.store(REQUEST_TONE, Ordering::Relaxed);
                }
                Some(Command::PlayWav) => {
                    board.apply(gates.power(Rail::Storage, true));
                    OUTPUT_REQUEST.store(OUTPUT_UP, Ordering::Relaxed);
                    REQUEST.store(REQUEST_WAV, Ordering::Relaxed);
                }
                Some(Command::Nfc) => {
                    board.apply(gates.power(Rail::Storage, true));
                    NFC_REQUEST.store(NFC_INVENTORY, Ordering::Relaxed);
                }
                Some(Command::Unlock) => {
                    board.apply(gates.power(Rail::Storage, true));
                    NFC_REQUEST.store(NFC_UNLOCK, Ordering::Relaxed);
                }
                Some(Command::ForceUnlock) => {
                    board.apply(gates.power(Rail::Storage, true));
                    NFC_REQUEST.store(NFC_FORCE_UNLOCK, Ordering::Relaxed);
                }
                Some(Command::CodecSet {
                    page,
                    register,
                    value,
                }) => {
                    if codec_override_set(page, register, value) {
                        esp_println::println!(
                            "teddiebox: codec page {page} register {register:#04x} -> {value:#04x} on next cinit"
                        );
                    } else {
                        esp_println::println!("teddiebox: no override slots left — cclr first");
                    }
                }
                Some(Command::CodecClear) => {
                    codec_overrides_clear();
                    esp_println::println!("teddiebox: codec overrides cleared");
                }
                Some(Command::CodecInit) => {
                    CODEC_REINIT.store(true, Ordering::Relaxed);
                }
                Some(Command::CodecDown) => {
                    CODEC_POWER_DOWN.store(true, Ordering::Relaxed);
                }
                Some(Command::Output(on)) => {
                    OUTPUT_REQUEST
                        .store(if on { OUTPUT_UP } else { OUTPUT_DOWN }, Ordering::Relaxed);
                }
                Some(Command::Speaker(on)) => {
                    SPEAKER_REQUEST.store(
                        if on { SPEAKER_UNMUTE } else { SPEAKER_MUTE },
                        Ordering::Relaxed,
                    );
                }
                Some(Command::NetScan) => {
                    NET_REQUEST.store(NET_SCAN, Ordering::Relaxed);
                }
                Some(Command::NetSsid(value)) => {
                    esp_println::println!("teddiebox: net ssid set to {value}");
                    set_ssid(value);
                }
                // Deliberately not echoed, the way `pw` is not.
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
                Some(Command::NetInsecure(insecure)) => {
                    set_insecure(insecure);
                    esp_println::println!(
                        "teddiebox: net certificates {} — takes effect on the next connection",
                        if insecure { "NOT checked" } else { "checked" }
                    );
                }
                Some(Command::Get(ruid)) => {
                    // Read and written in the one critical section: whatever
                    // `token`/`nfc`/`slix` last left behind is what this `get`
                    // sends, exactly as before — just captured as part of the
                    // one `FetchRequest` write rather than left for the fetch
                    // to go looking for later.
                    critical_section::with(|cs| {
                        let token = *TAG_TOKEN.borrow_ref(cs);
                        *FETCH_REQUEST.borrow_ref_mut(cs) = Some(FetchRequest {
                            ruid: u64::from_be_bytes(ruid),
                            token,
                        });
                    });
                    NET_REQUEST.store(NET_GET, Ordering::Relaxed);
                }
                Some(Command::StackReport) => match stack::high_water() {
                    None => esp_println::println!("teddiebox: stack was never painted"),
                    Some(used) => {
                        if used.exhausted {
                            // A floor, not an answer. Saying "deepest" here
                            // would be the same mistake that has already cost
                            // two bench sessions.
                            esp_println::println!(
                                "teddiebox: stack at least {} of {} bytes — the paint is gone \
                                 everywhere, so this is a floor",
                                used.bytes,
                                used.total
                            );
                        } else {
                            esp_println::println!(
                                "teddiebox: stack deepest {} of {} bytes, {} spare",
                                used.bytes,
                                used.total,
                                used.total.saturating_sub(used.bytes)
                            );
                        }
                    }
                },
                Some(Command::ReadToken) => {
                    board.apply(gates.power(Rail::Storage, true));
                    NFC_REQUEST.store(NFC_READ_TOKEN, Ordering::Relaxed);
                }
                Some(Command::PlayCache { directory, file }) => {
                    board.apply(gates.power(Rail::Storage, true));
                    CONTENT_DIRECTORY.store(directory, Ordering::Relaxed);
                    CONTENT_FILE.store(file, Ordering::Relaxed);
                    REQUEST.store(REQUEST_CACHE, Ordering::Relaxed);
                }
                Some(Command::NetTls) => {
                    NET_REQUEST.store(NET_TLS, Ordering::Relaxed);
                }
                Some(Command::NetStatus) => {
                    NET_REQUEST.store(NET_STATUS, Ordering::Relaxed);
                }
                Some(Command::ReadMemory { first, count }) => {
                    board.apply(gates.power(Rail::Storage, true));
                    NFC_MEM_RANGE.store(
                        (u32::from(first) << 8) | u32::from(count),
                        Ordering::Relaxed,
                    );
                    NFC_REQUEST.store(NFC_READ_MEMORY, Ordering::Relaxed);
                }
                Some(Command::Lock) => {
                    board.apply(gates.power(Rail::Storage, true));
                    NFC_REQUEST.store(NFC_LOCK, Ordering::Relaxed);
                }
                Some(Command::Plate(on)) => {
                    if !on {
                        PLATE_REPORT.store(true, Ordering::Relaxed);
                    }
                    if on {
                        board.apply(gates.power(Rail::Storage, true));
                    }
                    PLATE_POLLING.store(on, Ordering::Relaxed);
                    esp_println::println!(
                        "teddiebox: plate polling {}",
                        if on { "on" } else { "off" }
                    );
                }
                Some(Command::Password(value)) => {
                    NFC_PASSWORD.store(value, Ordering::Relaxed);
                    // Deliberately not echoed. It is a credential, and a bench
                    // capture is a file that outlives the session.
                    esp_println::println!("teddiebox: nfc password set");
                }
                Some(Command::PlayTaf) => {
                    board.apply(gates.power(Rail::Storage, true));
                    OUTPUT_REQUEST.store(OUTPUT_UP, Ordering::Relaxed);
                    REQUEST.store(REQUEST_TAF, Ordering::Relaxed);
                }
                Some(Command::PlaySound { file }) => {
                    board.apply(gates.power(Rail::Storage, true));
                    OUTPUT_REQUEST.store(OUTPUT_UP, Ordering::Relaxed);
                    CONTENT_DIRECTORY.store(LANGUAGE.content_directory(), Ordering::Relaxed);
                    CONTENT_FILE.store(file, Ordering::Relaxed);
                    REQUEST.store(REQUEST_CONTENT, Ordering::Relaxed);
                }
                Some(Command::PlayContent { directory, file }) => {
                    board.apply(gates.power(Rail::Storage, true));
                    OUTPUT_REQUEST.store(OUTPUT_UP, Ordering::Relaxed);
                    CONTENT_DIRECTORY.store(directory, Ordering::Relaxed);
                    CONTENT_FILE.store(file, Ordering::Relaxed);
                    REQUEST.store(REQUEST_CONTENT, Ordering::Relaxed);
                }
                Some(Command::DumpPcm { frames }) => {
                    board.apply(gates.power(Rail::Storage, true));
                    PCM_FRAMES.store(frames, Ordering::Relaxed);
                    REQUEST.store(REQUEST_PCM, Ordering::Relaxed);
                }
                Some(Command::Stop) => {
                    audio::STOP.store(true, Ordering::Relaxed);
                }
                Some(Command::Storage) => {
                    // The rail comes up here because this loop owns the pins.
                    // It stays up afterwards: the walk is a bench action, and a
                    // rail that drops under a card mid-read is a worse bug than
                    // one left on.
                    board.apply(gates.power(Rail::Storage, true));
                    REQUEST.store(REQUEST_WALK, Ordering::Relaxed);
                }
                None => {}
            }
        }

        Timer::after(BREATH_STEP).await;
    }
}
