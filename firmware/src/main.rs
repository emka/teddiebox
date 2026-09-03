#![no_std]
#![no_main]

mod audio;
mod led;
mod libc_shim;
mod net;
mod nfc;
mod pins;
mod storage;

use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};

use critical_section::Mutex as CsMutex;
use embassy_executor::Spawner;
use embassy_futures::select::{select, Either};
use embassy_time::{Duration, Instant, Timer};
use heapless::String;
use teddiebox_config::Config;
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
use teddiebox_core::power::{self, PackState};
use teddiebox_core::sounds::{Announcer, Language, Sound};
use teddiebox_core::tone;
use tlv320dac3100::Tlv320Dac3100;

use crate::pins::BoardPins;

/// Bytes of heap handed to the radio stack.
///
/// A guess, not a measurement, and the one number here most worth replacing
/// with one. `ControllerConfig::default()` asks the driver for 10 static RX
/// buffers of roughly 1.6 KB each, plus 32 dynamic RX and 32 dynamic TX
/// buffers, so 72 KiB is inside the plausible band and near the bottom of it.
/// Named rather than written inline so that the device plan's measurement has
/// exactly one place to land.
const RADIO_HEAP: usize = 72 * 1024;

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

/// Set when the console asks the radio for a scan.
///
/// Its own signal rather than a variant of `NFC_REQUEST`: the radio and the
/// reader share nothing but the console that drives them.
static NET_REQUEST: AtomicU8 = AtomicU8::new(REQUEST_NONE);
const NET_SCAN: u8 = 1;
const NET_UP: u8 = 2;
const NET_DOWN: u8 = 3;
const NET_STATUS: u8 = 4;

/// Credentials typed at the bench.
///
/// RAM only, gone at the next reset — the same treatment the SLIX password
/// gets and for the same reason: a credential belongs neither in the image nor
/// in the repository. **This is a stop-gap.** The design has these arriving
/// from the card's `CONFIG.TXT`, which is why `net::Radio::acquire` takes
/// a whole `Config` rather than two strings: when the card hands one over,
/// this static goes away and `net.rs` does not change at all.
static CREDENTIALS: CsMutex<RefCell<Credentials>> = CsMutex::new(RefCell::new(Credentials {
    ssid: String::new(),
    password: String::new(),
}));

struct Credentials {
    ssid: String<MAX_SSID>,
    password: String<MAX_PASSPHRASE>,
}

/// How long to wait for a DHCP lease before calling it a failure.
///
/// Association succeeding and addressing failing are different faults with
/// different causes, so they are waited for — and reported — separately.
const DHCP_TIMEOUT: Duration = Duration::from_secs(20);

fn set_ssid(value: String<MAX_SSID>) {
    critical_section::with(|cs| CREDENTIALS.borrow_ref_mut(cs).ssid = value);
}

fn set_password(value: String<MAX_PASSPHRASE>) {
    critical_section::with(|cs| CREDENTIALS.borrow_ref_mut(cs).password = value);
}

/// The credentials shaped the way the radio wants them.
///
/// `None` if either half is missing, because handing the driver an empty
/// string would fail association in a way that looks like a wrong password.
fn credentials() -> Option<Config> {
    critical_section::with(|cs| {
        let held = CREDENTIALS.borrow_ref(cs);
        if held.ssid.is_empty() || held.password.is_empty() {
            return None;
        }
        Some(Config {
            ssid: held.ssid.clone(),
            password: held.password.clone(),
            // Association has no use for it. The server is the download's
            // business and arrives with the rest of the config from the card.
            server: String::new(),
        })
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
    let mut bus = Some((spi, cs));
    let mut i2s_tx = Some(i2s_tx);
    let mut tone_buffer = Some(tone_buffer);
    let mut wav_buffer = Some(wav_buffer);
    // The tone's transfer stops when it is dropped, so it is parked here for
    // as long as the tone should play — which is until the box restarts.
    let mut _tone_transfer = None;

    loop {
        let request = REQUEST.swap(REQUEST_NONE, Ordering::Relaxed);
        if request == REQUEST_NONE {
            Timer::after(Duration::from_millis(100)).await;
            continue;
        }

        // The console loop raised the storage rail before setting the request.
        // Devices need their supply settled before they answer — a scan against
        // an unsettled rail is what invented an I2C device at 0x09 in step 4.
        if matches!(
            request,
            REQUEST_WALK | REQUEST_WAV | REQUEST_TAF | REQUEST_CONTENT | REQUEST_PCM
        ) && card.is_none()
        {
            let Some((spi, cs)) = bus.take() else {
                esp_println::println!("teddiebox: the card bus is gone — reboot to retry");
                continue;
            };
            Timer::after(Duration::from_millis(50)).await;
            match storage::Mounted::open(spi, cs, esp_hal::delay::Delay::new()) {
                Ok(mounted) => card = Some(mounted),
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

            REQUEST_WAV | REQUEST_TAF | REQUEST_CONTENT => {
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

                if request == REQUEST_WAV {
                    if let Err(reason) = audio::play_first_wav(card, tx, buffer).await {
                        esp_println::println!("teddiebox: playback failed — {reason}");
                    }
                } else {
                    let source = if request == REQUEST_CONTENT {
                        audio::Source::Content {
                            directory: CONTENT_DIRECTORY.load(Ordering::Relaxed),
                            file: CONTENT_FILE.load(Ordering::Relaxed),
                        }
                    } else {
                        audio::Source::First
                    };
                    // The hardware comes back, so playing again needs no
                    // reboot — which is what makes stopping worth anything.
                    let (outcome, tx, buffer) = audio::play_taf(card, tx, buffer, source).await;
                    if let Err(reason) = outcome {
                        esp_println::println!("teddiebox: playback failed — {reason}");
                    }
                    ANNOUNCING.store(false, Ordering::Relaxed);
                    i2s_tx = Some(tx);
                    wav_buffer = Some(buffer);
                    // Nothing is playing now, so the speaker has no business
                    // being driven.
                    OUTPUT_REQUEST.store(OUTPUT_DOWN, Ordering::Relaxed);
                }
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
async fn bring_up(radio: &mut net::Radio<'_>) {
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
async fn net(wifi: esp_hal::peripherals::WIFI<'static>) {
    let mut radio = net::Radio::new(wifi);

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
            NET_UP => bring_up(&mut radio).await,
            // `net status` and `net down` are answered inside `bring_up` while
            // it is running. Reaching them here means it is not.
            NET_STATUS | NET_DOWN => esp_println::println!("teddiebox: net is down"),
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
    while NFC_REQUEST.load(Ordering::Relaxed) == REQUEST_NONE {
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
            _ => {}
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
    spawner.spawn(net(p.WIFI).unwrap());

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
        "teddiebox: dl rb | t wav taf play <id>/<id> stop (loud) | sd | nfc pw slix slixp lock mem <2hex> <2hex> | net scan ssid <name> pw <pass> up down status | cinit cdown cset cclr out spk | pcm <2hex>"
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
