#![no_std]
#![no_main]

mod audio;
mod flash;
mod identity;
mod index;
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

use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, AtomicI8, AtomicU32, AtomicU8, Ordering};

use critical_section::Mutex as CsMutex;
use embassy_executor::Spawner;
use embassy_futures::select::{select, Either};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::{with_timeout, Duration, Instant, Timer};
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
use lis3dh::{clamped_threshold, regs as lis, ClickAxes, ClickAxis, ClickConfig, Lis3dh};
use teddiebox_board::{self as board, Gates, Rail};
use teddiebox_console::{Command, CommandWatch};
use teddiebox_core::i2c as bus;
use teddiebox_core::input::{self, Debounced, Edge};
use teddiebox_core::pipe::Pipe;
use teddiebox_core::place::PendingPlace;
use teddiebox_core::plate::{
    Answering, Placed, Presence, Seen, TagEvent, ARRIVALS_TO_AGREE, MISSES_TO_LEAVE,
};
use teddiebox_core::position::{self, MAX_POSITION};
use teddiebox_core::power;
use teddiebox_core::sounds::{Language, Sound};
use teddiebox_core::tone;

use teddiebox_core::{
    colour_for, db_for, Action, BatteryConfig, Core, CoreConfig, Ear, Event, Freshness, LedState,
    Output as AudioOutput, Position, PowerOffReason, TagUid, Unavailable, Volume, MAX_VOLUME,
};
use teddiebox_download::{
    Bytes, CardSays, ContentSink, Handshake, Landing, Pages, Placement, Step, Throttle, Writer,
};
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
//
// The version is stamped by build.rs from `git describe`, not taken from
// CARGO_PKG_VERSION — which is "0.1.0" and has never changed, so an update
// decided against it would compare a constant with itself for ever.
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

/// Prints on UART0 so a bench session can tell a running box from a hung one.
#[embassy_executor::task]
async fn heartbeat() {
    let mut ticks: u32 = 0;
    loop {
        // Nothing to do for the rest of this power-on: see `PARKED`.
        if PARKED.load(Ordering::Relaxed) {
            park_task().await;
        }

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

/// Everything the child did to the box, waiting for the reducer to be told
/// about it — an ear edge or a slap, not only ears.
///
/// A channel rather than a `Signal` like [`PLATE_TAG`], because these are
/// edges and not a state. `Core` pairs each `EarDown` with its `EarUp` to tell
/// a tap from a hold, so a lost press turns the next release into a long one
/// and a lost release leaves an ear held for ever. A signal keeps only the
/// last write, and both ears can settle inside one pass of the media loop.
///
/// Eight is far more than the two ears (and now the accelerometer) can
/// produce between two drains — the media task drains this once per pass
/// *and* once per decoded frame — so a full queue means something else is
/// wrong, and it says so rather than silently dropping an event that would
/// mislead the reducer.
static INPUT_EVENTS: Channel<CriticalSectionRawMutex, Event, 8> = Channel::new();

/// What the reducer last decided the LED should say.
///
/// The reducer runs in the media task; the LED belongs to the console loop,
/// which owns the LEDC channels. One byte between them, the same seam shape as
/// `OUTPUT_REQUEST` and `VOLUME_REQUEST` — and a state rather than a colour,
/// because which colour stands for which state is
/// `teddiebox_core::led::colour_for`'s business and is tested there.
static LED_REQUEST: AtomicU8 = AtomicU8::new(LedState::Booting.code());

/// Reports settled presses on the ears and the wake line.
///
/// Also counts the raw flips seen while each change settled: bench step 2 wants
/// the bounce duration of these particular switches measured, and a flip count
/// beside a known poll interval is the cheapest way to see it.
#[embassy_executor::task]
/// The wake line, polled.
///
/// The ears left this task on 2026-09-14 for [`ear`], which the GPIO hardware
/// wakes rather than a 2 ms sample. The wake line stays polled because `Core`
/// has no event for it: it is the button and the charger, nothing but this
/// print depends on it, and a sample missed under load costs a log line rather
/// than a chapter.
async fn inputs(wake: Input<'static>) {
    const POLL_MS: u64 = 2;

    let mut button = Debounced::released();
    // Raw flips seen since the last settled edge. Counted on the raw line
    // because counting polls that merely disagree with the settled state
    // yields DEBOUNCE_MS / POLL_MS every single time, which looks like data
    // and is not.
    let mut transitions = 0u32;
    let mut last_raw = false;

    loop {
        // Nothing to do for the rest of this power-on: see `PARKED`.
        if PARKED.load(Ordering::Relaxed) {
            park_task().await;
        }

        // The wake line is wanted by whoever is ending the session. Handed
        // over, and then parked *still holding it*: a task that returns drops
        // its pin, and a dropped `Input` does not leave a pad the way a
        // sleeping box needs it.
        if SLEEP_WANTED.load(Ordering::Relaxed) {
            break;
        }

        // `Debounced` takes a `u32` and handles its wrap.
        let now = Instant::now().as_millis() as u32;
        let pressed = input::wake_asserted(wake.is_high());

        if pressed != last_raw {
            transitions += 1;
            last_raw = pressed;
        }

        if let Some(edge) = button.update(pressed, now) {
            let label = match edge {
                Edge::Pressed => "pressed",
                Edge::Released => "released",
            };
            // One transition is the change itself; anything above that is
            // bounce. Sampled every POLL_MS, so bounce faster than that is
            // invisible here and reads as a clean edge.
            esp_println::println!("teddiebox: wake {label}, {transitions} raw transitions");
            transitions = 0;
        }

        Timer::after(Duration::from_millis(POLL_MS)).await;
    }

    critical_section::with(|cs| {
        WAKE_LINE.borrow_ref_mut(cs).replace(wake);
    });
    park_task().await;
}

/// One ear, held by the GPIO hardware instead of watched by a poll.
///
/// The poll this replaces sampled every 2 ms and still lost about one tap in
/// three while a story played. Measured at the bench on 2026-09-14: six
/// presses, five reported, twice over, and never a hold — a hold is long
/// enough to survive any gap. Decode takes 69% of real time and a card read
/// blocks for up to 13 ms, so this task could go tens of milliseconds without
/// running, and a press that began and ended inside one of those gaps left no
/// sample behind for `Debounced` to settle.
///
/// **The waits are on levels, not edges, and that is the whole point.**
/// `wait_for` clears any pending interrupt as it arms — see `unlisten_and_clear`
/// in esp-hal's `gpio/asynch.rs` — so an *edge* that happened while this task
/// was starved is already gone by the time it asks about one. A *level*
/// interrupt asserts immediately if the pin is holding that level, so a press
/// that happened during a gap completes the wait the instant it is armed. The
/// hardware keeps the fact and this task only has to turn up eventually, which
/// is the same bargain the accelerometer's click engine makes for a slap.
#[embassy_executor::task(pool_size = 2)]
async fn ear(which: Ear, name: &'static str, mut pin: Input<'static>) {
    loop {
        // Parked still holding the pin. A task that returns drops its `Input`,
        // and a dropped pad is not the one a sleeping box needs.
        if PARKED.load(Ordering::Relaxed) || SLEEP_WANTED.load(Ordering::Relaxed) {
            park_task().await;
        }

        pin.wait_for_low().await;
        let down_at = Instant::now().as_millis();
        esp_println::println!("teddiebox: {name} pressed");
        send_input(Event::EarDown(which, down_at));

        // Bounce on the way down needs nothing of its own: a release only
        // counts once the line has held high for `DEBOUNCE_MS`, so a press
        // that rattles fails that confirmation and goes on being a press.
        //
        // The wait for that release is also where a hold is noticed. This task
        // is the only thing awake at the moment a press *becomes* one — the
        // reducer sees events rather than time, and its tick is rate-limited
        // to once a second — so it says so, and the chapter changes with the
        // ear still down instead of when the child gives up and lets go.
        let mut bounces = 0u32;
        let mut held = false;
        loop {
            if !held {
                // Measured from the press, not from this iteration: a press
                // that rattled has already spent some of its 600 ms.
                let so_far = Instant::now().as_millis().saturating_sub(down_at);
                let remaining = u64::from(input::LONG_PRESS_MS).saturating_sub(so_far);
                if with_timeout(Duration::from_millis(remaining), pin.wait_for_high())
                    .await
                    .is_err()
                {
                    held = true;
                    // A box told `ears_skip = no` has stock's ears: volume and
                    // nothing else. The hold is still noticed and still
                    // reported, so a bench can see the press was long — it
                    // simply is not announced to the reducer, and the release
                    // that follows steps the volume like any other press.
                    if ears_skip() {
                        esp_println::println!("teddiebox: {name} held");
                        send_input(Event::EarHeld(which, Instant::now().as_millis()));
                    } else {
                        esp_println::println!("teddiebox: {name} held — ears_skip is off");
                    }
                    continue;
                }
            } else {
                pin.wait_for_high().await;
            }
            if with_timeout(
                Duration::from_millis(u64::from(input::DEBOUNCE_MS)),
                pin.wait_for_low(),
            )
            .await
            .is_err()
            {
                break;
            }
            bounces += 1;
        }

        // Dated when it happened rather than when it was confirmed. The
        // confirmation costs `DEBOUNCE_MS` by construction, and charging that
        // to the press would stretch every one of them by 20 ms — 3% of
        // `long_press_ms`, all of it in the direction that turns a tap into a
        // chapter skip.
        let up_at = Instant::now()
            .as_millis()
            .saturating_sub(u64::from(input::DEBOUNCE_MS));
        esp_println::println!("teddiebox: {name} released, {bounces} bounces");
        send_input(Event::EarUp(which, up_at));
    }
}

/// Hands one input event to the reducer, or says why it could not.
///
/// A full queue means something else is wrong: the media task drains it once
/// per pass *and* once per decoded frame, and eight is far more than two ears
/// and an accelerometer produce in between. So it is reported rather than
/// dropped quietly, because a lost release leaves an ear held for ever.
fn send_input(event: Event) {
    if INPUT_EVENTS.try_send(event).is_err() {
        esp_println::println!("teddiebox: ear queue full — {event:?} dropped");
    }
}

/// Set when the box has entered the setup portal instead of becoming a teddy
/// bear.
///
/// Read by [`sense`], which is the one task setup mode keeps. Everything that
/// would consume what `sense` produces — the reducer, the LED loop, the
/// shutdown — lives below the branch that sets this and is never reached, so
/// the readings have a console to go to and nowhere else.
static SETUP_MODE: AtomicBool = AtomicBool::new(false);

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
    // The LED reads the pack, so unplugging a charger should change the colour
    // while the hand is still on the cable.
    const SAMPLE_EVERY: Duration = Duration::from_secs(2);
    // Printing stays where it was, purely so a bench capture is readable.
    const PRINT_EVERY: u8 = 5;
    let mut since_printed = 0u8;
    let mut batlog_ticks: u8 = 0;

    loop {
        // Nothing to do for the rest of this power-on: see `PARKED`.
        if PARKED.load(Ordering::Relaxed) {
            park_task().await;
        }

        // Raw counts as well as millivolts. The conversion rests on an
        // assumed attenuation and on GPIO9 measuring the pack rather than
        // something downstream of it, and a millivolt figure alone cannot
        // tell a wrong assumption from a flat battery.
        let pack_raw = adc.read_blocking(&mut battery);
        let charger_raw = adc.read_blocking(&mut charger);
        let pack_mv = power::battery_mv(pack_raw);
        let charger_mv = power::charger_mv(charger_raw);

        // Published for the one reader that cannot wait for the reducer: a
        // box deciding, before it powers anything up, whether the wake it just
        // had is worth answering. The counter is what makes a *fresh* reading
        // distinguishable from the same one read twice.
        PACK_MV.store(pack_mv, Ordering::Relaxed);
        PACK_SAMPLES.store(
            PACK_SAMPLES.load(Ordering::Relaxed).wrapping_add(1),
            Ordering::Relaxed,
        );

        // The reducer decides the LED, and until now it was never told the one
        // thing the LED most needs to say. Both go through the same channel
        // the ears use: the media task drains it once per loop pass and once
        // per decoded frame, so a warning is not held behind a story that has
        // half an hour left to run.
        //
        // Except in setup mode, where that task does not exist. Posting to a
        // queue nothing drains fills it in sixteen seconds and then reports a
        // dropped event every two, which buries the readings this task was
        // kept running to produce.
        let charging = power::charger_present(charger_raw);
        if !SETUP_MODE.load(Ordering::Relaxed) {
            for event in [
                Event::Battery {
                    pack_mv: pack_mv as u16,
                    under_load: PLAYING.load(Ordering::Relaxed),
                },
                Event::Charger(charging),
            ] {
                // A full queue is somebody else's bug — this is the slowest
                // producer on it, at one pair every two seconds.
                if INPUT_EVENTS.try_send(event).is_err() {
                    esp_println::println!("teddiebox: pack event dropped — the queue is full");
                }
            }
        }

        since_printed += 1;
        if since_printed >= PRINT_EVERY {
            since_printed = 0;
            esp_println::println!(
                "teddiebox: pack {pack_mv} mV (raw {pack_raw}), charger {charger_mv} mV (raw {charger_raw})"
            );
        }

        // One line, no judgement. The header is printed when the log is armed
        // so a capture can be pasted straight into a plotter.
        let every = BATLOG_EVERY.load(Ordering::Relaxed);
        if every > 0 {
            batlog_ticks += 1;
            // SAMPLE_EVERY.as_millis() is u64, so the comparison is done in
            // u64 throughout. A `seconds` that is not a multiple of the 2 s
            // sample period rounds up to the next tick — coarser than asked
            // for, never finer, and that is fine for a bench capture.
            if u64::from(batlog_ticks) * SAMPLE_EVERY.as_millis() >= u64::from(every) * 1_000 {
                batlog_ticks = 0;
                esp_println::println!(
                    "batlog,{},{pack_raw},{pack_mv},{},{charger_raw}",
                    Instant::now().as_millis(),
                    PLAYING.load(Ordering::Relaxed) as u8
                );
            }
        }

        Timer::after(SAMPLE_EVERY).await;
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

/// Identifies the accelerometer, then streams its axes, runs the part's
/// click engine, and raises [`Event::Slap`] when a click lands on the axis
/// `board::side_for_click` maps to a side.
///
/// Both candidate addresses are tried because 0x18 is shared with the audio
/// codec, which acknowledges and answers something that is not an identity
/// register.
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
            if dac.set_volume_db(BOOT_VOLUME_DB).is_err() {
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
    // Starts full, not zero like `since_report`: the first pass should read
    // the register rather than wait three, so a box that boots with a jack
    // already in does not spend ~600 ms with `HEADPHONES_IN` still saying
    // nothing is plugged in — the codec bring-up that follows this task reads
    // that static.
    let mut since_detect: u32 = HEADSET_DETECT_EVERY;
    // Starts in agreement with `HEADPHONES_IN`, so a box that boots with
    // nothing plugged in raises no event at all. Kept here rather than read
    // back from the static because `hp 1` writes that one, and a forced
    // routing must survive every poll that reads the same register value it
    // read before — which is the whole escape hatch if this jack turns out
    // not to reach the codec's detect pin.
    let mut last_detect = false;
    // Logged on transition, not on every poll, for the same reason the slap
    // threshold below is: this console is the box's only user interface, and a
    // codec that failed bring-up would otherwise bury everything else on it.
    let mut detect_failed = false;
    // Same reason again: with no card mounted nothing drains `INPUT_EVENTS`,
    // so once it fills the send below fails on every poll that has a change to
    // deliver — and it keeps having one, because the retry is the point.
    let mut detect_dropped = false;
    // `armed_threshold` always tracks the last value a write was *attempted*
    // with; `armed` is only set by a write that actually succeeded. Guarding
    // the re-arm below on `armed` — not just on the threshold changing — is
    // what lets a failed boot arm be recovered later: a failed write leaves
    // `armed` false, so the next pass (or the operator re-typing the very
    // same threshold the failure message names) still retries, instead of
    // comparing equal to a value that was only ever hoped for.
    //
    // `confirmed_threshold` is a separate, narrower fact: the last value a
    // write actually landed at, for display only. It must never be set from
    // an attempt — only from a success — because the click print reads it as
    // "what the part is running", and during a failed re-arm window
    // `armed_threshold` holds a number the part never accepted.
    let mut armed_threshold = SLAP_THRESHOLD.load(Ordering::Relaxed);
    let mut armed = false;
    let mut failure_reported = false;
    let mut confirmed_threshold: Option<u8> = None;
    let mut slap_refractory: u8 = 0;
    let mut armed_limit: u8 = SLAP_TIME_LIMIT.load(Ordering::Relaxed);
    if accel.init().is_err() {
        esp_println::println!("teddiebox: LIS3DH would not start");
        return;
    }

    // All three axes, because which one a slap lands on is unmeasured; the
    // board decides which of them means a side, and discards the rest.
    match accel.enable_click(ClickConfig {
        // Y and Z, not X. Measured upright at the bench 2026-09-13: standing
        // as a child uses it, the box reads `accel -16000 256 64` — gravity
        // entirely on X, so X is the VERTICAL axis and a side slap cannot
        // produce it. Every X click recorded before this came from the box
        // lying tipped, or from a vertical shock. The PCB sits at 45 degrees
        // to the slap axis, so a side slap lands on Y and Z together and the
        // part latches whichever crosses first. X stays off so setting the
        // box down is not a chapter skip.
        axes: SLAP_AXES,
        threshold: armed_threshold,
        time_limit: SLAP_TIME_LIMIT.load(Ordering::Relaxed),
    }) {
        Ok(()) => {
            armed = true;
            confirmed_threshold = Some(armed_threshold);
        }
        Err(_) => {
            esp_println::println!(
                "teddiebox: LIS3DH would not take a click config — \
                 slaps will not be detected until the threshold is set from the console"
            );
            // Already told the operator once, for this threshold; the loop
            // below retries every pass without repeating itself until either
            // the write succeeds or a different threshold is asked for.
            failure_reported = true;
        }
    }

    loop {
        // Nothing to do for the rest of this power-on: see `PARKED`.
        if PARKED.load(Ordering::Relaxed) {
            park_task().await;
        }

        let speaker = SPEAKER_REQUEST.swap(0, Ordering::Relaxed);
        {
            if let request @ (SPEAKER_MUTE | SPEAKER_UNMUTE | SPEAKER_RESUME) = speaker {
                let bus = accel.release();
                let mut dac = Tlv320Dac3100::new(bus, tlv320dac3100::DEFAULT_ADDRESS);
                let outcome = match request {
                    SPEAKER_RESUME => dac.resume_speaker(&mut dac_delay),
                    SPEAKER_UNMUTE => dac.unmute_speaker(&mut dac_delay),
                    _ => dac.mute_speaker(),
                };
                match outcome {
                    Ok(()) => esp_println::println!(
                        "teddiebox: speaker {}",
                        match request {
                            SPEAKER_RESUME => "powered and unmuted",
                            SPEAKER_UNMUTE => "unmuted",
                            _ => "muted",
                        }
                    ),
                    Err(_) => esp_println::println!("teddiebox: speaker would not change"),
                }
                let bus = dac.release();
                accel = Lis3dh::new(bus, address);
                continue;
            }
        }
        if HEADPHONE_REPORT.swap(false, Ordering::Relaxed) {
            let bus = accel.release();
            let mut dac = Tlv320Dac3100::new(bus, tlv320dac3100::DEFAULT_ADDRESS);
            let reading = dac
                .headset_detect_raw()
                .and_then(|detect| Ok((detect, dac.headset_status_raw()?)));
            let bus = dac.release();
            accel = Lis3dh::new(bus, address);
            match reading {
                // Four separate facts, printed apart on purpose. `detect` is
                // register 67: D7 set is detection switched on, and D6-D5 name
                // the last headset it saw — which it keeps naming long after
                // that headset is gone, so it cannot answer "is one in now".
                // `status` is register 46, whose D4 is the jack itself, and
                // that is what the box reads. They are printed side by side
                // because their disagreement is the normal case and reading
                // only one of them is how a removal went missing on
                // 2026-09-22. Detection off makes both a constant 00, which
                // is also what an empty jack looks like — D7 in `detect` is
                // what separates them. The routing is the box's own belief,
                // which `hp 1` can set without either register moving.
                Ok((detect, status)) => esp_println::println!(
                    "teddiebox: headset detect {:#04x} status {:#04x}, says {}, routing to {}",
                    detect,
                    status,
                    if status & tlv320dac3100::HEADSET_INSERTED != 0 {
                        "in"
                    } else {
                        "out"
                    },
                    if HEADPHONES_IN.load(Ordering::Relaxed) {
                        "headphones"
                    } else {
                        "speaker"
                    }
                ),
                Err(_) => esp_println::println!("teddiebox: headset detect unreadable"),
            }
            continue;
        }
        let volume = VOLUME_REQUEST.swap(NO_VOLUME, Ordering::Relaxed);
        if volume != NO_VOLUME {
            let bus = accel.release();
            let mut dac = Tlv320Dac3100::new(bus, tlv320dac3100::DEFAULT_ADDRESS);
            match dac.set_volume_db(volume) {
                Ok(()) => esp_println::println!("teddiebox: codec volume {volume} dB"),
                Err(_) => esp_println::println!("teddiebox: codec volume would not change"),
            }
            let bus = dac.release();
            accel = Lis3dh::new(bus, address);
            continue;
        }
        let output = OUTPUT_REQUEST.swap(0, Ordering::Relaxed);
        if let request @ (OUTPUT_DOWN | OUTPUT_UP) = output {
            let bus = accel.release();
            let mut dac = Tlv320Dac3100::new(bus, tlv320dac3100::DEFAULT_ADDRESS);
            let up = request == OUTPUT_UP;
            let speaker = !HEADPHONES_IN.load(Ordering::Relaxed);
            let outcome = if up {
                dac.start_output(&mut dac_delay, speaker)
            } else {
                dac.stop_output()
            };
            match outcome {
                Ok(()) => esp_println::println!(
                    "teddiebox: codec output {}{}",
                    if up { "up" } else { "down" },
                    if up && !speaker { " (headphones)" } else { "" }
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
                    let _ = dac.set_volume_db(BOOT_VOLUME_DB);
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
        match accel.take_click() {
            Ok(Some(click)) => {
                // Printed unconditionally: a click is rare, and this is the
                // instrument the bench calibrates with. `raw` is here because
                // the datasheet does not pin CLICK_SRC's bits down.
                // More than one axis bit set means `take_click`'s X-then-Y-
                // then-Z priority picked one and dropped the rest. Harmless
                // for skipping a chapter — only one axis maps to a side — but
                // a trap for calibration, which is here to find out WHICH axis
                // a slap lands on and would read X every time. Say so out
                // loud rather than leaving it in the raw byte to be noticed.
                let axes_set = (click.raw & 0b111).count_ones();
                // The threshold printed here is `confirmed_threshold`, not
                // `armed_threshold`: during a failed re-arm window the two
                // disagree, and this line is what a calibration log is read
                // back against after the session, once the "NOT applied"
                // line has long since scrolled away. A click before any
                // write has ever succeeded — only possible if the part is
                // arming inconsistently — says so rather than guessing.
                match confirmed_threshold {
                    Some(threshold) => esp_println::println!(
                        "teddiebox: click {:?} {} raw {:#04x} threshold {}{}",
                        click.axis,
                        if click.negative { "-" } else { "+" },
                        click.raw,
                        clamped_threshold(threshold),
                        if axes_set > 1 { " MULTI-AXIS" } else { "" }
                    ),
                    None => esp_println::println!(
                        "teddiebox: click {:?} {} raw {:#04x} threshold unconfirmed{}",
                        click.axis,
                        if click.negative { "-" } else { "+" },
                        click.raw,
                        if axes_set > 1 { " MULTI-AXIS" } else { "" }
                    ),
                }
                let axis = match click.axis {
                    ClickAxis::X => board::Axis::X,
                    ClickAxis::Y => board::Axis::Y,
                    ClickAxis::Z => board::Axis::Z,
                };
                if let Some(side) = board::side_for_click(axis, click.negative) {
                    if slap_refractory > 0 {
                        // Printed, not silent: during calibration the rebounds
                        // are the measurement — they say whether the window is
                        // long enough and the threshold low enough.
                        esp_println::println!("teddiebox: slap ignored, within refractory");
                    } else {
                        slap_refractory = SLAP_REFRACTORY_POLLS;
                        if INPUT_EVENTS.try_send(Event::Slap(side)).is_err() {
                            esp_println::println!("teddiebox: input queue full, slap dropped");
                        }
                    }
                }
            }
            Ok(None) => {}
            Err(_) => esp_println::println!("teddiebox: LIS3DH click read failed"),
        }

        slap_refractory = slap_refractory.saturating_sub(1);

        let wanted = SLAP_THRESHOLD.load(Ordering::Relaxed);
        let wanted_limit = SLAP_TIME_LIMIT.load(Ordering::Relaxed);
        if !armed || wanted != armed_threshold || wanted_limit != armed_limit {
            // A different threshold being asked for is itself news, worth a
            // fresh failure line even if the last attempt already reported
            // one — otherwise typing a new value while the part is stuck
            // looks like it was ignored rather than retried.
            let threshold_changed = wanted != armed_threshold || wanted_limit != armed_limit;
            armed_threshold = wanted;
            armed_limit = wanted_limit;
            // `armed` only becomes true on a confirmed write: the bench
            // trusts the success line to mean the part is actually running at
            // `wanted`, and a discarded error here would print a success the
            // box never delivered. Clearing it on failure also means the next
            // pass tries again rather than believing it is done — including
            // when the retry asks for the exact value the failure message
            // named.
            match accel.enable_click(ClickConfig {
                axes: SLAP_AXES,
                threshold: wanted,
                time_limit: wanted_limit,
            }) {
                Ok(()) => {
                    armed = true;
                    failure_reported = false;
                    confirmed_threshold = Some(wanted);
                    esp_println::println!(
                        "teddiebox: slap threshold {} limit {}",
                        clamped_threshold(wanted),
                        wanted_limit
                    );
                }
                Err(_) => {
                    armed = false;
                    // Logged on transition, not on every attempt: this loop
                    // retries every 200 ms, and the console is the box's only
                    // user interface. Repeating the failure every pass would
                    // bury the click prints calibration depends on.
                    if !failure_reported || threshold_changed {
                        esp_println::println!(
                            "teddiebox: slap threshold {wanted} NOT applied, LIS3DH write failed"
                        );
                        failure_reported = true;
                    }
                }
            }
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
        since_detect += 1;
        if since_detect >= HEADSET_DETECT_EVERY {
            since_detect = 0;
            // The bus goes back to the accelerometer before anything is
            // decided, so a full input queue cannot leave the codec holding it.
            let bus = accel.release();
            let mut dac = Tlv320Dac3100::new(bus, tlv320dac3100::DEFAULT_ADDRESS);
            let reading = dac.headphones_connected();
            let bus = dac.release();
            accel = Lis3dh::new(bus, address);
            match reading {
                Ok(now) if now != last_detect => {
                    detect_failed = false;
                    // The two halves of this advance at different moments on
                    // purpose. `HEADPHONES_IN` is the routing the codec
                    // bring-up reads, so it is stored the moment the register
                    // moves and nothing downstream can lose it. `last_detect`
                    // is what makes a change worth reporting at all, so it may
                    // only move once the reducer has actually been told:
                    // advancing it on a send that failed would leave the box
                    // routed one way and the reducer playing the other, with
                    // no later poll left to notice the disagreement.
                    HEADPHONES_IN.store(now, Ordering::Relaxed);
                    if INPUT_EVENTS.try_send(Event::Headphones(now)).is_err() {
                        if !detect_dropped {
                            esp_println::println!(
                                "teddiebox: input queue full, jack change dropped"
                            );
                            detect_dropped = true;
                        }
                    } else {
                        last_detect = now;
                        detect_dropped = false;
                        esp_println::println!(
                            "teddiebox: headphones {}",
                            if now { "in" } else { "out" }
                        );
                    }
                }
                Ok(_) => detect_failed = false,
                Err(_) => {
                    if !detect_failed {
                        esp_println::println!("teddiebox: headset detect unreadable");
                        detect_failed = true;
                    }
                }
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

/// `CLICK_THS`, live so the bench can sweep it without a reflash.
///
/// **40, measured 2026-09-13**: one step is 62 mg at the +/-8 g full scale
/// `init` selects, so about 2.5 g. Eight slaps — four on each face — were
/// caught eight times with the correct direction every time. It is the
/// sensitive end of a bracket: 48 was silent through handling but missed a
/// slap and mis-signed another, while 40 lets roughly one handling knock per
/// session through. 40 was chosen, favouring the gesture
/// working over the occasional lost place.
static SLAP_THRESHOLD: AtomicU8 = AtomicU8::new(40);
/// Polls to ignore clicks for after accepting a slap, at `ACCEL_POLL_MS` each:
/// two is 400 ms, the refractory window the deleted `GestureConfig` carried.
///
/// One physical slap is not one click. The box rocks back off the hit, and at a
/// threshold low enough to catch the slap through the PCB's 45-degree mounting
/// the rebound crosses too — in the OPPOSITE sign, so it reads as a slap on the
/// other side and skips the wrong way. Measured at the bench 2026-09-13: five
/// slaps on one side gave two correct skips and one backwards one.
///
/// The latch is still read and cleared during the window; only the action is
/// suppressed. Leaving it unread would hold a stale click for the next slap.
const SLAP_REFRACTORY_POLLS: u8 = 2;

/// Which axes the click engine watches. See the note at the boot-time arm.
const SLAP_AXES: ClickAxes = ClickAxes {
    x: false,
    y: true,
    z: false,
};
/// `TIME_LIMIT`, in ODR periods: 3 is 60 ms at the 50 Hz `init` sets.
static SLAP_TIME_LIMIT: AtomicU8 = AtomicU8::new(4);

/// How loud the box plays until an ear says otherwise.
///
/// No longer a number chosen at the bench: it is the level of the step
/// `VolumeModel` starts a box on, so the codec's initial state and the
/// reducer's belief about it are one number rather than two that happen to
/// agree. Without that, the first tap would step from a level the ladder does
/// not contain.
///
/// It is still deliberately quiet — see `teddiebox_core::db_for` for what the
/// ladder is anchored on, which is the same listening that set the -35 dB this
/// replaces.
const BOOT_VOLUME_DB: i8 = db_for(AudioOutput::Speaker, Volume(MAX_VOLUME / 2));

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
const REQUEST_CRC: u8 = 8;

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
/// The two travel together for the same reason [`Seen::Figure`] keeps
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
    /// Whether this is a question rather than a download.
    ///
    /// A probe borrows the whole of the fetch's machinery — the same request,
    /// the same route, the same token, the radio raised and dropped the same
    /// way — and differs only in what it asks for and what it does with the
    /// answer. Carrying the difference here rather than in a second request
    /// keeps one association lifecycle instead of two, and makes it
    /// impossible for a probe and a fetch to be queued at the same time.
    probe: bool,
}

/// The fetch about to be raised on [`NET_GET`], written whole in one go.
///
/// Two writers — the console `get` command and the plate's `RequestContent`
/// action — and each writes its own [`FetchRequest`] immediately before
/// raising the request, never touching the other's fields separately.
static FETCH_REQUEST: CsMutex<RefCell<Option<FetchRequest>>> = CsMutex::new(RefCell::new(None));

/// The net task's answer to a probe, and the figure it is about, as one value.
///
/// One value rather than a flag, a length and a ruid in three atomics, so a
/// reader can never see an answer without knowing whose it is, or a length
/// from one answer beside the flag of another. The figure is carried for the
/// same reason the fetch outcome carries one: an answer that arrives after the
/// figure was lifted or swapped belongs to nobody.
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

/// A cached file the server has contradicted, as `CACHE/<dir>/<file>`.
///
/// Armed when a probe comes back stale, and read once by the download that
/// answers it. Nothing else is touched: the story stays playable until the
/// refetch actually starts writing, because a box that discards what it has
/// the moment it hears of something newer has a story fewer either way the
/// download goes.
static STALE_DIR: AtomicU32 = AtomicU32::new(0);
static STALE_FILE: AtomicU32 = AtomicU32::new(0);
static STALE_ARMED: AtomicBool = AtomicBool::new(false);

/// Whether this cache entry is the one a probe has just contradicted.
///
/// Consumed: a plan that has read it has answered it, and a second download of
/// the same file has no reason to start from zero again.
fn take_stale(dir: u32, file: u32) -> bool {
    if !STALE_ARMED.load(Ordering::Relaxed)
        || STALE_DIR.load(Ordering::Relaxed) != dir
        || STALE_FILE.load(Ordering::Relaxed) != file
    {
        return false;
    }
    STALE_ARMED.store(false, Ordering::Relaxed);
    true
}

/// The question the box is waiting on the server for, if any.
///
/// Touched only by the media task — `perform` asks, the loop settles — so it
/// has one writer. It is a static rather than a loop local for the same reason
/// `ASKED` is: `perform` is reached through `apply` from eight places, and
/// threading it through all of them buys nothing.
static REVALIDATION: CsMutex<RefCell<teddiebox_download::Revalidation>> =
    CsMutex::new(RefCell::new(teddiebox_download::Revalidation::new()));

/// Which figures have been asked about since the box booted.
///
/// RAM, like the position slots, and lost on every reset — which is the whole
/// cadence: one boot is roughly one session, because the box switches itself
/// off after five idle minutes.
static ASKED: CsMutex<RefCell<teddiebox_download::Asked>> =
    CsMutex::new(RefCell::new(teddiebox_download::Asked::new()));

/// Whether this figure has already been asked about this session.
pub(crate) fn already_asked(tag: TagUid) -> bool {
    critical_section::with(|cs| ASKED.borrow_ref(cs).contains(tag.ruid()))
}

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

/// How often the producer looks for an answer while a handshake is in
/// progress.
///
/// Finer than [`teddiebox_download::RETRY_MS`] on purpose: this is how long a
/// figure waits past the moment the card actually answers, and the card
/// answering is the common case. The re-ask cadence and the deadline are the
/// download crate's to decide.
///
/// **A floor, not a period.** While a story plays the media task blocks the
/// executor for hundreds of milliseconds at a time, so this timer comes back
/// every ~106 ms — measured on the box on 2026-09-23. That is why the
/// handshake is handed [`Instant::now`] rather than this number.
const HANDSHAKE_POLL_MS: u64 = 10;

/// What the card said, as [`CardSays::as_offset`] encodes it.
///
/// Written by the card's owner during [`DOWNLOAD_PREPARING`] and read by the
/// producer, which cannot touch the card itself. Encoded and decoded by the
/// download crate rather than here, so the sentinel and the reason it is safe
/// sit next to the tests that hold them.
static DOWNLOAD_FROM: AtomicU32 = AtomicU32::new(0);

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
/// The access point turned the box away: the card's passphrase is not its one.
const FETCH_REFUSED: u8 = 4;

/// The byte that carries one of the reducer's reasons between the two tasks.
///
/// Exhaustive on purpose: a reason added to [`Unavailable`] and not given a
/// byte here is a compile error rather than a figure that silently announces
/// the wrong fault. That is the whole reason this is a function and not a
/// match written out at each of the two call sites.
const fn outcome_for(why: Unavailable) -> u8 {
    match why {
        Unavailable::Unreachable => FETCH_UNREACHABLE,
        Unavailable::Refused => FETCH_REFUSED,
        Unavailable::NoContent => FETCH_NO_CONTENT,
    }
}

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
///
/// The outcome and the figure it belongs to are two atomics, so they are
/// written together under a critical section and [`take_fetch_outcome`] reads
/// them the same way. Nothing today can land between the two stores — one
/// executor, one core, no await in here — but "an outcome attributed to the
/// wrong figure" is the exact bug the identity was added to prevent, and
/// pairing them costs a critical section on a path that runs once per
/// download.
fn fetch_ended(outcome: u8) {
    critical_section::with(|_| {
        FETCH_OUTCOME_RUID.store(FETCH_ACTIVE_RUID.load(Ordering::Relaxed), Ordering::Relaxed);
        FETCH_OUTCOME.store(outcome, Ordering::Relaxed);
    });
}

/// Records how a download ended, unless something already has.
///
/// Two paths reach the end of the same fetch — the net task's, which knows
/// *why* it failed, and the media task's, which only knows the file stopped
/// short. The first to speak wins: overwriting `NoContent` with the coarser
/// `Unreachable` would throw away the difference between a server that has no
/// story and one that could not be reached, which is the difference between
/// the two things the box can say out loud.
///
/// Returns whether it said anything, and takes the same critical section as
/// [`fetch_ended`], so the check and the store cannot be separated.
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

/// Records that a probe is over and taught the box nothing.
///
/// **Silence is the one answer a question may not have.** The reducer holds a
/// placed figure without playing anything until it hears back, so a probe that
/// ends without publishing leaves the box quiet with a story sitting on its
/// card — which is exactly the failure revalidation was not allowed to cause.
/// Every net-task path that can end a probe without an answer calls this. A
/// `Nothing` about a question that has already settled is dropped by
/// [`teddiebox_download::Revalidation`], so calling it twice is harmless.
fn probe_ended(ruid: u64) {
    post_answer(ruid, teddiebox_download::Answer::Nothing);
}

/// Which of the reducer's two words a failed fetch deserves.
///
/// A `404` or a `403` means the server answered and has no story for this
/// figure; teddyCloud's `403` is what a request without a usable token gets,
/// which is the same thing from the box's point of view. Everything else —
/// the name, the socket, the handshake, a fault of the server's own — means
/// the server was not reached, whatever the reason. The console keeps the
/// reason; this is only what the box can say out loud.
///
/// [`tls::Error::Abandoned`] never reaches here — it is answered before this is
/// called, because a transfer that was told to stop is not a story that could
/// not be obtained. If that arm is ever removed, the catch-all below will turn
/// every lifted figure into an announced network fault.
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
/// The same shape as [`why_unavailable`]: a fact from the network turned into
/// the one word the reducer can act on. Every path that is not "the server
/// stated a different length" answers `Current`, because none of them is
/// evidence against a file that is sitting on the card and plays — and
/// revalidation may never make a working box worse than it was offline.
fn freshness_of(
    answer: teddiebox_download::Answer,
    tag: TagUid,
    card: Option<&storage::Mounted>,
) -> Freshness {
    let teddiebox_download::Answer::Length(total) = answer else {
        return Freshness::Current;
    };
    let Some(card) = card else {
        // No card is not a judgement about the file; it is the reason this
        // question cannot be answered at all.
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
        // Armed here, where the contradiction is known, and consumed by the
        // download the reducer is about to ask for.
        let path = teddiebox_download::content_path(tag.0);
        STALE_DIR.store(path.directory, Ordering::Relaxed);
        STALE_FILE.store(path.file, Ordering::Relaxed);
        STALE_ARMED.store(true, Ordering::Relaxed);
        Freshness::Stale
    } else {
        Freshness::Current
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

        // **A sidecar the server has contradicted vouches for nothing.** Read
        // as it stands, it says the file is complete and the whole download
        // turns into "the card already holds all of it" — which would play the
        // very copy the probe just found stale. Dropping it needs no new rule:
        // a file nothing vouches for is refetched from zero, truncating what
        // is there, which is exactly what a stale file deserves.
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
            let says = plan_download(card);
            DOWNLOAD_FROM.store(says.as_offset(), Ordering::Relaxed);
            // Only answers a question still being asked. Reading the card can
            // take longer than the producer is prepared to wait, and an answer
            // stored after it has given up would leave this state machine in
            // `DOWNLOAD_PLANNED` with nobody left to move it on — the media
            // loop then polls at its fast download cadence for ever. The same
            // rule `Handshake::card_answered` applies at the other end, for
            // the same reason.
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
        let written = active.writer.watermark().0;
        esp_println::println!(
            "teddiebox: get wrote {} bytes, crc32 {:08X}",
            written,
            active.crc.finish()
        );
        card.close_file(active.file);
        *write = None;

        // A transfer that stopped and a story that is ready are not the same
        // statement, and this used to make the second whenever the first was
        // true: an aborted or failed download was announced as complete, and
        // the reducer opened a fragment. On 2026-09-07 that was 1,089,536
        // bytes of a 38,349,983-byte story, saved from playing only because
        // the decoder refused it.
        //
        // The arithmetic decides instead — where this body went, plus what
        // landed, against what the server said the file is.
        let whole = teddiebox_download::is_whole(
            DOWNLOAD_AT.load(Ordering::Relaxed),
            written,
            match DOWNLOAD_TOTAL.load(Ordering::Relaxed) {
                0 => None,
                total => Some(total),
            },
        );
        if whole {
            // Here rather than where the fetch returned: a story is ready when
            // it is on the card, not when the last byte left the socket.
            // Playing on the earlier answer would open a file the writer has
            // not finished.
            fetch_ended(FETCH_COMPLETED);
        } else {
            esp_println::println!(
                "teddiebox: get stopped short — {} of {} bytes, not vouching for it",
                DOWNLOAD_AT.load(Ordering::Relaxed).saturating_add(written),
                DOWNLOAD_TOTAL.load(Ordering::Relaxed)
            );
            // Not if the download was abandoned: a figure that has been
            // lifted is not owed an explanation, and the reducer has already
            // moved on to having no figure at all.
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

/// Whether a command that exists to take a box apart may run.
///
/// These reach the box over UART0 with no gate beyond having the case open,
/// and the worst of them — `otaboot` onto a blank slot — leaves a box that
/// only a J100 cold boot recovers. That is an acceptable risk at a bench and a
/// pointless one in a box on a shelf, so the `bench` feature decides, and the
/// refusal says which build the box is running rather than looking like a
/// parse failure.
///
/// Whether this image carries the console at all, beyond `dl`.
///
/// On unless `TEDDIEBOX_RELEASE` asks otherwise, so an ordinary `just
/// firmware` or `just flash` builds what a bench session wants and nothing
/// about a working day changes. `build.rs` turns that variable into the `bench`
/// cfg and already knows to rebuild when it changes; the environment rather
/// than a cargo feature because that is how this build takes its other three
/// decisions — the language, the SLIX password and the version.
///
/// A `const` rather than a `#[cfg]`, so the dispatch below stays one match and
/// stays exhaustive: a command added later still has to be wired up or the
/// build fails. It has to be a `const` and not a function returning the same
/// answer — a function is called from the one place either way, but only a
/// constant lets the optimiser drop the arms behind it. Measured: a helper
/// function made the release build *larger* than the bench one, and gating
/// eleven arms individually saved 3,928 bytes where gating the dispatch saves
/// 18,848.
const BENCH: bool = cfg!(bench);

/// Says why a command did nothing, so a release box does not look like it
/// failed to parse the line.
fn not_in_this_build() {
    esp_println::println!(
        "teddiebox: this is a release image — only `dl` is here, to flash a bench one"
    );
}

/// The box's settings: what the card said, and what the bench typed over it.
///
/// Anything typed here is RAM only, gone at the next reset — the same treatment
/// the SLIX password gets and for the same reason: a credential belongs neither
/// in the image nor in the repository. The precedence rule between the two
/// sources lives in [`teddiebox_config::Settings`], where it is tested without
/// a box; this static is only the lock around it.
static SETTINGS: CsMutex<RefCell<Settings>> = CsMutex::new(RefCell::new(Settings::new()));

/// How long to wait for a DHCP lease before calling it a failure.
///
/// Association succeeding and addressing failing are different faults with
/// different causes, so they are waited for — and reported — separately.
const DHCP_TIMEOUT: Duration = Duration::from_secs(20);

fn set_ssid(value: String<MAX_SSID>) {
    critical_section::with(|cs| SETTINGS.borrow_ref_mut(cs).set_ssid(value));
}

fn set_password(value: String<MAX_PASSPHRASE>) {
    critical_section::with(|cs| SETTINGS.borrow_ref_mut(cs).set_password(value));
}

/// Publishes what the card said.
///
/// The media task owns the card and calls this once at boot; the console
/// writes over it afterwards, which is what makes a mistyped card
/// diagnosable at the bench without pulling it.
fn set_configuration(value: Config) {
    critical_section::with(|cs| SETTINGS.borrow_ref_mut(cs).take_card(value));
}

/// The credentials shaped the way the radio wants them.
///
/// `None` if either half is missing, because handing the driver an empty
/// string would fail association in a way that looks like a wrong password.
fn credentials() -> Option<Config> {
    critical_section::with(|cs| SETTINGS.borrow_ref(cs).credentials().cloned())
}

fn set_ears_skip(value: bool) {
    critical_section::with(|cs| SETTINGS.borrow_ref_mut(cs).set_ears_skip(value));
}

/// Whether a held ear should skip a chapter, as the card last said.
///
/// Read per press rather than held in a local, because the card is mounted
/// lazily: an ear pressed before the first mount would otherwise pin this
/// task to the boot default for the rest of the session.
fn ears_skip() -> bool {
    critical_section::with(|cs| SETTINGS.borrow_ref(cs).config().ears_skip)
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

/// The SLIX privacy password baked in at build time, or zero for none.
///
/// From `TEDDIEBOX_SLIX_PASSWORD` in the build environment — `.envrc.local`
/// on the bench, a repository secret in CI — and never from the repository
/// itself. The same `option_env!` treatment `TEDDIEBOX_LANGUAGE` gets, and
/// `build.rs` declares both so that changing one is not silently ignored by a
/// cached build.
///
/// **This does put a credential in the image**, which the console-only
/// arrangement it replaces deliberately did not: anybody holding a built
/// binary can read it back out. Chosen anyway, on 2026-09-14, because the
/// alternative was worse in practice — a password that has to be typed every
/// session is one an unattended box does not have, and a tag whose password
/// is missing is *silent*, which looks exactly like an empty plate, a wrong
/// password and a broken antenna. It nearly sank the first autonomous fetch.
///
/// An unset or empty variable leaves this zero, which is what a build without
/// the secret gets and exactly how the box behaved before: nothing is
/// unlocked until somebody types `pw`.
const BUILT_IN_PASSWORD: u32 = match option_env!("TEDDIEBOX_SLIX_PASSWORD") {
    None => 0,
    Some(text) => {
        if text.is_empty() {
            0
        } else {
            match teddiebox_console::hex::u32_from_hex(text.as_bytes()) {
                Some(value) => value,
                // A build is the right place to find this out. The bench's
                // way of finding out is a tag that says nothing at all.
                None => panic!("TEDDIEBOX_SLIX_PASSWORD must be exactly eight hex digits"),
            }
        }
    }
};

/// The SLIX privacy password in force.
///
/// Starts as [`BUILT_IN_PASSWORD`] and is replaced by whatever `pw` types, so
/// a bench can still work with a tag the image was not built for. RAM only:
/// it is never written to the card.
static NFC_PASSWORD: AtomicU32 = AtomicU32::new(BUILT_IN_PASSWORD);

/// The block range a pending `mem` carries: first block in the high byte,
/// block count in the low one.
///
/// Packed into one word rather than kept in two, so the reader task cannot
/// observe a first block from one command beside a count from the next.
static NFC_MEM_RANGE: AtomicU32 = AtomicU32::new(0);

/// Whether the reader polls the plate on its own. On at boot.
///
/// **The default is what a child's box does**, because a box that ignores
/// every figure until somebody types at it is not a box. It used to be off,
/// to keep a poller that unlocks tags by itself from contaminating a bench
/// measurement — and `plate off` still buys exactly that, for the session
/// that asks. What made the old default untenable is that a release image
/// answers no command but `dl`, so this was the one setting a finished box
/// could never reach: it played its jingle and then ignored every figure for
/// ever.
///
/// Switching it on by default was gated on a measurement. Polling an *empty*
/// plate cost 49 DMA restarts in 70 s against 0 with the poller off — worse
/// than the radio — and the bench note of 2026-09-07 said not to change this
/// default until that path was made cheap. It was, the same day: 49 -> 5, and
/// 0 with a figure present. The remaining 5 belong to an empty plate, and
/// lifting a figure now pauses the story (`Playback::on_tag_absent`), so an
/// empty plate no longer coincides with anything playing — which is the only
/// condition under which a restart can be heard.
static PLATE_POLLING: AtomicBool = AtomicBool::new(true);
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
static PLATE_TAG: Signal<CriticalSectionRawMutex, Seen> = Signal::new();

/// Set when the box is about to stop being able to write the card.
///
/// The console loop decides to shut down; the media task owns the card. This
/// is the one bit between them, and the shutdown waits briefly for it to clear
/// rather than taking the place down with it.
static FLUSH_PLACE: AtomicBool = AtomicBool::new(false);

/// The place a figure was lifted from, before the card knows about it.
///
/// The policy itself lives in [`PendingPlace`], where the host can drive it:
/// which lift displaces which, and which ending clears the slot, are decided
/// by rules that had two defects found by re-reading them. What is left here
/// is the shared-access wrapper and the card, neither of which a test can have.
static PENDING_PLACE: CsMutex<RefCell<PendingPlace>> =
    CsMutex::new(RefCell::new(PendingPlace::new()));

/// Where the next story should start.
///
/// Written by `perform` when the reducer decides to play, taken by the media
/// task when it opens the file. Taken rather than read: a story the console
/// starts after one a figure started must not inherit the figure's place.
static PLAY_FROM: CsMutex<RefCell<Position>> = CsMutex::new(RefCell::new(Position::Start));

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
///
/// Owned by the playback it names, which is why the two statics below travel
/// with it: the flag used to be cleared by whichever playback happened to end
/// next. A story running when the pack went critical would clear it on its own
/// ending, and the shutdown then dropped the rails a fraction of a second into
/// the sentence that was still being spoken — the common path, because a pack
/// reaches the cutoff while playing far more often than while idle.
static ANNOUNCING: AtomicBool = AtomicBool::new(false);

/// Which `CONTENT/<dir>/<file>` [`ANNOUNCING`] is about, so the playback that
/// ends can say whether it is the announcement.
static ANNOUNCING_DIRECTORY: AtomicU32 = AtomicU32::new(0);
static ANNOUNCING_FILE: AtomicU32 = AtomicU32::new(NO_SOUND);

/// The level the codec should be playing at, in whole dB, or [`NO_VOLUME`].
///
/// The codec lives in the task that shares its I2C bus with the accelerometer,
/// and the reducer lives in the media task, so this is the same kind of seam
/// `OUTPUT_REQUEST` and `SPEAKER_REQUEST` already are. It carries dB rather
/// than a step because dB is the codec's own language: which step that came
/// from is the reducer's business and nothing the codec task should have an
/// opinion about.
static VOLUME_REQUEST: AtomicI8 = AtomicI8::new(NO_VOLUME);

/// No level is waiting. Outside the codec's -63.5..=+24 dB range, so it can
/// never collide with a real request.
const NO_VOLUME: i8 = i8::MIN;

/// Set once the codec is configured and would be heard if it were driven.
///
/// The start-up jingle waits on this. `init` spends 400 ms letting the output
/// drivers ramp, and a jingle that begins before then loses its first second
/// to a codec that is not listening yet.
static CODEC_READY: AtomicBool = AtomicBool::new(false);

/// Set once the card has been mounted for the first time this boot.
///
/// Card mount is lazy — the ordinary boot only does it once the start-up
/// jingle asks to play — so this is the second half `ota::mark_valid` waits
/// on, alongside `CODEC_READY`.
///
/// Setup mode's own card-open path deliberately never sets this — see the
/// branch below that mounts the card for `portal::run`. That is what lets a
/// bench session force the revert path on purpose, without ever flashing a
/// broken image. The same absence has a cost outside the bench: holding both
/// ears to enter setup mode shortly after a genuinely good update landed —
/// say, to fix Wi-Fi credentials — leaves that update's slot unconfirmed, and
/// it gets reverted on the next boot even though nothing was wrong with it.
/// Accepted rather than fixed, because it self-heals via the next
/// revalidation or download, and because setup mode existing at all is
/// already an emergency recovery path.
static CARD_MOUNTED: AtomicBool = AtomicBool::new(false);

/// Set when the box has said it is turning off, and must therefore do it.
///
/// `BatteryCritical` is not a warning, it is an announcement — "battery is
/// critical, turning off now" — so it is the one sound with an obligation
/// attached. A box that says this and keeps playing has told a child
/// something untrue, which is worse than saying nothing.
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

/// Set once the box has actually parked — rails down, nothing left to say.
///
/// Every periodic task reads this and stops for good. Until it existed the
/// box parked only in the sense that a child could not see or hear it: the
/// heartbeat went on printing once a second, `sense` went on converting two
/// ADC channels every two seconds, and the reducer went on being ticked, all
/// on a pack the park exists to save. Measured at the bench on 2026-09-08,
/// three minutes of a parked box cost 125 heartbeat lines and 13 ADC rounds.
///
/// Set after the card flush and after the rails go down, because the tasks
/// this stops are the ones that carry those out.
static PARKED: AtomicBool = AtomicBool::new(false);

/// Set by `awake on`, to hold the idle timeout off for a bench session.
///
/// Reported to the reducer as use, rather than checked at the point of
/// shutdown: the reducer owns the policy, and a second place that could
/// veto a park would be a second thing to reason about when one of them
/// gets it wrong.
///
/// Off at boot and lost on every reset — a box that stays awake because a
/// previous session said so is measuring the wrong thing. Unlike
/// `PLATE_POLLING`, whose default is what a child's box does, there is no
/// child's box that wants this on.
static STAY_AWAKE: AtomicBool = AtomicBool::new(false);

/// The most recent pack reading, in millivolts, and how many have been taken.
///
/// Zero samples means the sense task has not answered yet, which is not the
/// same as a flat pack and must not be read as one.
static PACK_MV: AtomicU32 = AtomicU32::new(0);
static PACK_SAMPLES: AtomicU32 = AtomicU32::new(0);

/// Waits for fresh pack readings and answers whether they agree it is empty.
///
/// **Two, not the four `readings_to_agree` asks for.** Four exists to stop one
/// implausible sample *moving the level* — the first pack reading this project
/// ever took was 9453 mV from three NiMH cells — and two consecutive readings
/// already kill that. Four of them is eight seconds of a box sitting dark
/// while a child holds an ear, and the cost of being wrong here is small: the
/// box goes back to sleep and the next press asks again.
///
/// `None` as soon as a reading is at or above the cutoff, and `None` if the
/// sense task says nothing at all — booting normally is the safe answer to a
/// question that was never answered.
async fn pack_says_empty() -> Option<u32> {
    const READINGS_TO_AGREE: u8 = 2;
    let cutoff = u32::from(BatteryConfig::default().cutoff_mv);
    let mut seen = PACK_SAMPLES.load(Ordering::Relaxed);
    let mut agreed = 0u8;

    // Ten seconds, against a sense task that samples every two.
    for _ in 0..200 {
        let taken = PACK_SAMPLES.load(Ordering::Relaxed);
        if taken != seen {
            seen = taken;
            let mv = PACK_MV.load(Ordering::Relaxed);
            if mv >= cutoff {
                return None;
            }
            agreed += 1;
            if agreed >= READINGS_TO_AGREE {
                return Some(mv);
            }
        }
        Timer::after(Duration::from_millis(50)).await;
    }
    None
}

/// Whether the end of a session is deep sleep rather than a park.
///
/// **On by default since 2026-09-14, when the wake was proven on hardware.**
/// The box slept on command, stayed silent, and came back on an ear press
/// reporting `rst:0x5 (DSLEEP)`. That settles which of the two endings is the
/// right default, and it is not the park: a parked box answers nothing but the
/// switch — it ignored `dl` and cost a power cycle twice, once that same
/// morning — while a sleeping one answers an ear, which is the thing a child
/// has.
///
/// Sleep current is still unmeasured, and deliberately not a reason to wait.
/// A park draws tens of milliamps; whatever sleep draws, it is not *more*, and
/// it is wakeable. The measurement decides how good this is, not whether it
/// beats parking.
///
/// `autosleep off` turns it back into a park for one session — for a bench
/// that wants a box which cannot disappear mid-measurement. Lost on every
/// reset, like `PLATE_POLLING` and `STAY_AWAKE`, so the default is what a
/// child's box does.
static AUTO_SLEEP: AtomicBool = AtomicBool::new(true);

/// Asks the task that polls the wake line to hand the pin over.
///
/// The ending belongs to the console loop — it owns the rails, the codec and
/// the `LPWR` peripheral — but the pin belongs to `inputs`, which polls it.
/// Arming and entering have to happen close together and in one task: esp-hal
/// treats a level interrupt as single-shot and clears the same bit sleep entry
/// reads, so an ear pressed between the two turns a wakeable sleep into a
/// panic. Moving the pin is cheaper than splitting the job.
static SLEEP_WANTED: AtomicBool = AtomicBool::new(false);

/// Where the wake line waits between the two tasks.
///
/// `inputs` stops polling the moment it puts the pin here and parks with the
/// ears still held, so nothing drops a pin the sleep depends on. Put back by
/// [`sleep_now`] whenever the sleep does not happen, so `sleep` can be typed
/// again once whoever was holding an ear lets go.
static WAKE_LINE: CsMutex<RefCell<Option<Input<'static>>>> = CsMutex::new(RefCell::new(None));

/// Takes the wake line, arms it, and enters deep sleep. Returns only on
/// failure, and then the box is still awake and still dark.
///
/// The caller has already gone dark: this says nothing a child can hear and
/// nothing they can see.
async fn sleep_now(lpwr: &mut Option<LPWR<'static>>) -> Result<(), &'static str> {
    SLEEP_WANTED.store(true, Ordering::Relaxed);
    let mut held = None;
    for _ in 0..40 {
        held = critical_section::with(|cs| WAKE_LINE.borrow_ref_mut(cs).take());
        if held.is_some() {
            break;
        }
        Timer::after(Duration::from_millis(5)).await;
    }
    let Some(mut wake) = held else {
        return Err("the wake line never arrived");
    };

    // A level wake on a line already at its wake level ends the sleep the
    // instant it begins, and an ear holds this line down for as long as it is
    // held — so the press that asked for this has to be over first. Bounded,
    // because a line held for ten seconds is a fault and not a slow finger.
    for _ in 0..200 {
        if wake.is_high() {
            break;
        }
        Timer::after(Duration::from_millis(50)).await;
    }

    // Said before arming, not after. The line has to reach the UART before the
    // chip stops clocking it, and that wait is dead time in which an ear press
    // can clear the very bit sleep entry reads — so the waiting happens while
    // nothing is armed yet, and arming is the last thing before entering.
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
                WAKE_LINE.borrow_ref_mut(cs).replace(wake);
            });
            Err(reason)
        }
    }
}

/// Everything a child can see or hear, off, in the order the hardware needs.
///
/// One sequence, because there are two ways to reach it — the box deciding to
/// stop, and a bench asking it to — and two copies of an order that matters
/// would eventually stop matching.
async fn go_dark(board: &mut BoardPins<'_>, gates: &mut Gates, rgb: Option<&led::Rgb<'_>>) {
    quieten_codec().await;
    // Before the rail goes down, not after. The LED is held by LEDC, which
    // keeps driving its three channels with no processor involvement, and
    // `Gates::led` refuses once the peripherals rail is down — so dropping the
    // rail first left the parked box showing a steady green, seen at the bench
    // on 2026-09-08.
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
/// `pending` never completes, so the executor never schedules the task again
/// — no timer, no poll, no wakeup at all. Deliberately not a `return`: that
/// would drop the pins and buses the task owns, and a dropped `Input` does
/// not necessarily leave its pin the way a parked box wants it. Holding them
/// in a future that never finishes keeps every pin exactly as the park left
/// it.
async fn park_task() {
    core::future::pending::<()>().await;
}

/// Which content file `play` names, as two halves of `CONTENT/<dir>/<file>`.
static CONTENT_DIRECTORY: AtomicU32 = AtomicU32::new(0);
static CONTENT_FILE: AtomicU32 = AtomicU32::new(0);

/// How many frames `pcm` should print.
static PCM_FRAMES: AtomicU8 = AtomicU8::new(0);

/// Seconds between `batlog` lines, or zero for off.
static BATLOG_EVERY: AtomicU8 = AtomicU8::new(0);

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

/// What is plugged into the headphone jack, as far as the box knows.
///
/// **The only place the firmware records this.** The detect poll in `motion`
/// writes it, `hp 1` / `hp 0` overwrite it, and the codec bring-up reads it.
/// `Action::SetOutput` deliberately does not: that action drives
/// `SPEAKER_REQUEST` and nothing else, because two statics that can disagree
/// about one fact is the shape the 2026-09-18 review objected to once.
static HEADPHONES_IN: AtomicBool = AtomicBool::new(false);

/// Ask the motion task to read the headset-detect register and say what it
/// says. A request rather than a print here, because the codec is on the
/// motion task's bus and nothing else may touch it.
static HEADPHONE_REPORT: AtomicBool = AtomicBool::new(false);

/// Full passes of the `motion` loop between headset-detect reads.
///
/// The loop naps `ACCEL_POLL_MS`, so three passes is about 600 ms; plug to
/// silence is that plus the codec's 128 ms debounce. Full passes, not
/// iterations: the request handlers above `continue`, which skips the timer,
/// so counting every iteration would poll far faster than this says.
///
/// Not worth making faster. Polling harder is the wrong reflex on this box —
/// an empty plate's polling already costs more than the radio does.
const HEADSET_DETECT_EVERY: u32 = 3;

/// A pending speaker change: 0 nothing, 1 mute, 2 unmute, 3 resume.
static SPEAKER_REQUEST: AtomicU8 = AtomicU8::new(0);
const SPEAKER_MUTE: u8 = 1;
const SPEAKER_UNMUTE: u8 = 2;
/// Power the class-D amplifier if it is not already up, then unmute it.
///
/// Distinct from `SPEAKER_UNMUTE`, which moves only the driver's mute bit.
/// A story that began with headphones in never powered the amplifier — that
/// click is deliberately not paid at the start of such a story — so the
/// unplug has to power it before there is anything to unmute into. Kept apart
/// from the plain unmute because `spk 1` exists so a bench can hear powering
/// and unmuting separately, and folding them together would take that
/// instrument away.
const SPEAKER_RESUME: u8 = 3;

/// A pending output power change: 0 nothing, 1 down, 2 up.
static OUTPUT_REQUEST: AtomicU8 = AtomicU8::new(0);
const OUTPUT_DOWN: u8 = 1;
const OUTPUT_UP: u8 = 2;

/// Register overrides applied to the codec's start-up sequence.
///
/// The table and its three rules live in `tlv320dac3100`, where they are
/// tested without a box; this is the lock around one. It was an array of
/// packed `AtomicU32` here, lock-free so a console write and a codec bring-up
/// could not collide — which bought nothing a critical section around six
/// `Option`s does not, and cost the rules a form anything could test.
static CODEC_OVERRIDES: CsMutex<RefCell<tlv320dac3100::Overrides>> =
    CsMutex::new(RefCell::new(tlv320dac3100::Overrides::new()));

fn codec_override_set(page: u8, register: u8, value: u8) -> bool {
    critical_section::with(|cs| {
        CODEC_OVERRIDES
            .borrow_ref_mut(cs)
            .set(page, register, value)
    })
}

fn codec_overrides_clear() {
    critical_section::with(|cs| CODEC_OVERRIDES.borrow_ref_mut(cs).clear());
}

/// Copies `table` into `out`, applying any override for each register.
fn codec_apply_overrides<'a>(
    table: &[(u8, u8, u8)],
    out: &'a mut [(u8, u8, u8)],
) -> &'a [(u8, u8, u8)] {
    critical_section::with(|cs| CODEC_OVERRIDES.borrow_ref(cs).apply(table, out))
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

/// Reads the card's copy of the certificate authority, so the server can be
/// verified even on a box whose identity is missing or invalid.
///
/// The box's own certificate and key are not read here, or anywhere off the
/// card: they come from flash, read once at boot in [`identity::load`].
fn read_anchor(card: &storage::Mounted) {
    let mut certificate = [0u8; tls::CERT_BYTES];

    // `ca`, not `identity`: the box's own certificate and key come from flash
    // and report under `identity`, and these two used to be one function. A
    // console whose worth is that every line means something should not use
    // one word for both the box proving who it is and the box checking who it
    // is talking to.
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

    read_anchor(card);

    let mut buffer = [0u8; CONFIG_BUFFER];
    match card.read_config(&mut buffer) {
        Ok(config) => {
            // The passphrase is not printed, here or anywhere. Its length is
            // enough to tell a truncated card from a wrong one.
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
/// The card is written for the *outgoing* figure only: its place is about to
/// become unreachable, and this is the last moment anything knows it.
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
/// Asked before the card, because the slot is newer by construction: it is
/// written the moment a figure comes off, and the card only learns later.
pub(crate) fn held_place(tag: TagUid) -> Option<u32> {
    critical_section::with(|cs| PENDING_PLACE.borrow_ref(cs).held(tag))
}

/// Drops a story's held place, by the path it lives at.
///
/// Called where the place has stopped meaning anything — a story that reached
/// its end. Taking the ruid rather than the tag because the media task knows
/// where it is playing from and not which figure asked.
fn forget_place(ruid: u64) {
    critical_section::with(|cs| PENDING_PLACE.borrow_ref_mut(cs).forget(ruid));
}

/// Puts whatever RAM is holding onto the card.
///
/// Called where the slot is about to be lost for good. Idempotent by
/// construction: the slot is emptied, so a second call writes nothing.
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

        // The parental ceiling, the stepping and which ladder a step is on are
        // all the reducer's; the dB arrives already said in the codec's units.
        Action::SetVolume { step, db } => {
            esp_println::println!("teddiebox: volume step {} — {db} dB", step.0);
            VOLUME_REQUEST.store(db, Ordering::Relaxed);
        }

        // Muting the class-D driver, and nothing else. In particular not
        // `HEADPHONES_IN`: what is plugged in has one writer, and two statics
        // that can disagree about one fact is the shape the 2026-09-18 review
        // objected to. Muted rather than powered down because powering an
        // output stage is what clicks, measured by ear, whether or not the
        // driver in front of it is muted.
        Action::SetOutput(output) => {
            esp_println::println!(
                "teddiebox: output {}",
                match output {
                    AudioOutput::Speaker => "speaker",
                    AudioOutput::Headphones => "headphones",
                }
            );
            // `SPEAKER_RESUME` rather than a plain unmute: a story that began
            // with headphones in never powered the class-D amplifier, so the
            // unmute would have nothing to unmute into. Powering it is
            // audible, and this is the moment to pay for it — somebody has
            // just pulled a plug out.
            SPEAKER_REQUEST.store(
                match output {
                    AudioOutput::Speaker => SPEAKER_RESUME,
                    AudioOutput::Headphones => SPEAKER_MUTE,
                },
                Ordering::Relaxed,
            );
        }

        // Which chapter is playing, and what the ends of the story mean, are
        // the decoder's — it is the only thing that knows either. This says
        // only which way.
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
            // Written whole, in the one call: see `FetchRequest` for why the
            // ruid and the token it authorises never travel separately.
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

        Action::PlayPrompt(prompt) => match Sound::for_prompt(prompt) {
            Some(sound) => SOUND_REQUEST.store(sound.file(), Ordering::Relaxed),
            // No file on this card has been identified for this sentence, and
            // substituting another would have the box say something true about
            // the wrong thing. The console hears it; the child hears nothing.
            None => esp_println::println!(
                "teddiebox: plate no sound identified for {prompt:?} — saying nothing"
            ),
        },

        Action::SavePosition { tag, pos } => {
            // Into RAM, not onto the card. A child lifts a figure and puts it
            // back over and over, and each of those is a place worth keeping
            // but not a place worth a card write — the card only has to know
            // once the box is about to forget.
            if let Position::Exact { page } = pos {
                remember_in_ram(index, tag, page);
            }
        }

        Action::SetLed(state) => LED_REQUEST.store(state.code(), Ordering::Relaxed),

        // The one place the box decides to stop. It used to be decided twice —
        // here in the reducer's words, where nothing acted on it, and again by
        // a second model of the pack in the console loop, which is what
        // actually happened. The console loop now only samples.
        //
        // Raised rather than acted on directly: the console loop owns the
        // rails, the codec and the card's last write, and it waits for the
        // announcement raised alongside this to finish before it acts. Said
        // here rather than there, because this is the only place that knows
        // which of the two authorities decided.
        Action::PowerOff(reason) => {
            esp_println::println!(
                "teddiebox: plate powering off — {}",
                match reason {
                    PowerOffReason::PackEmpty => "the pack is below the cutoff",
                    PowerOffReason::Idle => "nothing has used the box",
                }
            );
            // A pack below the cutoff must stop being driven, and a story left
            // running drives it for another half hour — which is the discharge
            // the cutoff exists to prevent, on cells with no protection
            // circuit. Safe for the announcement queued alongside this: every
            // playback clears `STOP` before its first frame.
            //
            // A stop is not an ending. `play_taf` answers `Finish::Stopped`,
            // and the completion path — which clears the story's place and
            // tells the reducer it ended — is reached only on `Finish::Ended`,
            // so the child's place survives being switched off.
            audio::STOP.store(true, Ordering::Relaxed);
            SHUTTING_DOWN.store(true, Ordering::Relaxed);
        }

        // The rest are not reachable, and saying what was wanted is the
        // honest half of that.
        other => esp_println::println!("teddiebox: plate not wired — {other:?}"),
    }
}

/// Drains the reader's signal into what the media task believes is on the
/// plate.
///
/// All that is left here is the signal, which is the part a host test cannot
/// have; the figure and its token are replaced together by [`Placed::observe`]
/// where a test can drive it. They are two halves of one identity, and letting
/// them drift apart is the bug [`FetchRequest`] exists to make unrepresentable.
///
/// Called from two places — once per pass of the media loop, and once per
/// frame while a story plays — because a loop pass is a whole story long and a
/// figure lifted during one must not wait for it to end.
fn take_plate_event(placed: &mut Placed) -> Option<Event> {
    let event = placed.observe(PLATE_TAG.try_take()?);
    // A lift ends any question about the figure, here where the lift is first
    // seen. Settled later in the same pass, an answer would be judged against
    // a figure already put back — before the reducer has heard it is back.
    if event == Event::TagAbsent {
        critical_section::with(|cs| REVALIDATION.borrow_ref_mut(cs).withdrawn());
    }
    Some(event)
}

/// Hands the reducer every ear edge that has settled since it was last asked.
///
/// Drained rather than read one at a time, because `Core` pairs a press with
/// its release and leaving half a pair in the queue is what turns the next tap
/// into a hold.
///
/// Needs the card only because `Core::handle` takes an index for every event,
/// not because an ear does. A box whose card would not mount has nothing to
/// play and so nothing to turn down, which is what makes that acceptable
/// rather than merely convenient.
fn apply_ear_events(reducer: &mut Core, card: &storage::Mounted, token: Option<[u8; 32]>) {
    while let Ok(event) = INPUT_EVENTS.try_receive() {
        apply(reducer, card, event, token);
    }
}

/// Tells the reducer what happened and carries out what it decides.
///
/// The index is built here and dropped at the end: it borrows the card, and
/// the rest of the media loop needs the card unborrowed — which is the whole
/// reason it is this cheap to construct.
fn apply(reducer: &mut Core, card: &storage::Mounted, event: Event, token: Option<[u8; 32]>) {
    // Reported on every event rather than once a second with the tick: this is
    // what decides whether a press is ambiguous, and a press arriving in the
    // second after `ears skip off` was typed must not be judged by the old
    // answer. Reading it costs a critical section and no card access.
    reducer.note_ears_skip(ears_skip());
    let index = CardIndex::new(card);
    for action in reducer.handle(event, &index) {
        perform(action, &index, token);
    }
}

/// The core's only clock, fed at most once a second.
///
/// Called from both the top of the media loop and the per-frame `attend`
/// closure inside a playing story, so there has to be one rate limit rather
/// than two: a tick per frame would be a tick every few milliseconds, and this
/// project has measured audible DMA restarts from far less extra work than
/// that in the media loop. A second is far finer than a five-minute timeout
/// needs.
fn feed_tick(
    reducer: &mut Core,
    card: &storage::Mounted,
    token: Option<[u8; 32]>,
    last_fed: &mut u64,
) {
    let now = Instant::now().as_millis();
    if now.saturating_sub(*last_fed) >= 1_000 {
        *last_fed = now;
        // Reported beside the clock it guards, because the reducer's state
        // answers for figures on the plate and nothing else. A console `play`
        // or `taf` runs with it sitting at `Idle`, and a `batlog` run is hours
        // of deliberate use during which the box may make no sound at all —
        // both of which the idle timeout would otherwise cut short. The fact
        // is reported here; the policy is the reducer's.
        reducer.note_in_use(
            PLAYING.load(Ordering::Relaxed)
                || BATLOG_EVERY.load(Ordering::Relaxed) > 0
                || STAY_AWAKE.load(Ordering::Relaxed),
        );
        apply(reducer, card, Event::Tick(now), token);
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
    // The token it arrived with is held in the same value rather than in a
    // shared static: it is read exactly once, when the reducer actually asks
    // for a fetch, and bundled with that figure's ruid into one
    // `FetchRequest` write — so nothing typed at the console between now and
    // then can attach itself to this figure's fetch.
    let mut placed = Placed::empty();
    // The last millisecond a `Tick` was fed to the reducer, so `feed_tick` can
    // rate-limit itself across both call sites — the top of this loop and the
    // per-frame `attend` closure inside a playing story.
    let mut last_tick_fed: u64 = 0;

    loop {
        // Nothing to do for the rest of this power-on: see `PARKED`.
        if PARKED.load(Ordering::Relaxed) {
            park_task().await;
        }

        // Fed before the request is read, so a `Play` decided here is picked
        // up on the same pass rather than the next one.
        let mut events: [Option<Event>; 3] = [None, None, None];

        // Asked before anything else, because the answer is only useful while
        // the card is still writable.
        if FLUSH_PLACE.swap(false, Ordering::Relaxed) {
            if let Some(card) = card.as_ref() {
                flush_place(&CardIndex::new(card), "the box is stopping");
            }
        }

        events[0] = take_plate_event(&mut placed);

        // A download has ended somewhere this task cannot see. Read every pass
        // so a stale answer cannot arrive later attached to a different figure.
        let (outcome, outcome_ruid) = take_fetch_outcome();
        if outcome != FETCH_NOTHING {
            match placed.answering(outcome_ruid) {
                // The figure this outcome was for is still on the plate: the
                // reducer's guard checks the same identity again before
                // acting, but the event it sees is at least about the right
                // figure.
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
                // A figure is on the plate, but this outcome belongs to a
                // different one — a console `get` that finished while
                // something else sits here. Attributing it to the figure on
                // the plate is exactly the bug this identity exists to
                // prevent, so the reducer never sees it.
                Answering::AnotherFigure => esp_println::println!(
                    "teddiebox: plate ignoring a fetch outcome for {outcome_ruid:016X} — \
                     not the figure on the plate"
                ),
                // Nobody is waiting. Already the harmless case this read
                // exists to guarantee.
                Answering::NoFigure => {}
            }
        }

        // The server has answered a question about a cached story. Read every
        // pass, and guarded by identity for the same reason the fetch outcome
        // is: an answer that arrives after the figure was lifted or swapped
        // belongs to nobody.
        // Each call into the machine is its own short critical section, and
        // nothing prints inside one: a console line at 115200 baud is
        // milliseconds with interrupts off.
        let mut settled = None;
        if let Some((ruid, answer)) = take_answer() {
            settled =
                critical_section::with(|cs| REVALIDATION.borrow_ref_mut(cs).answered(ruid, answer));
            if settled.is_none() {
                // An answer to a question that is already over — the patience
                // ran out, or an earlier answer settled it. Acting on it is
                // what once armed the stale flag for a figure playing its card
                // copy, so it goes no further than the console.
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
                    // Remembered whatever the answer was. A server that was
                    // unreachable a moment ago is unreachable for the rest of
                    // a five-minute session, and asking it again on the next
                    // placement spends the radio to be told the same thing.
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

        // Every pass, not only when a figure moved: a tap has nothing to do
        // with the plate and must not wait for one.
        if let Some(mounted) = card.as_ref() {
            apply_ear_events(&mut reducer, mounted, placed.token());
        }

        // The core's only clock. Withheld until now because `Core` emits
        // `PowerOff` on a tick once the idle timeout passes and nothing acted
        // on it — so feeding it did nothing, and wiring it without the arm in
        // `perform` would have been a shutdown nobody could see coming.
        //
        // This alone would not be enough: a story plays for the whole length
        // of `play_taf`, below, so a tick fed only here would go stale for as
        // long as half an hour. `feed_tick` is called again from inside that
        // call's `attend` closure for exactly that reason.
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
                // the next playback to start. Nor must a skip: an ear held
                // down with nothing playing would otherwise lose the first
                // chapter of whatever is played next.
                audio::STOP.store(false, Ordering::Relaxed);
                audio::SKIP.store(audio::SKIP_NONE, Ordering::Relaxed);

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
                    //
                    // The ears are here for the same reason and it is the
                    // whole point of them: turning a story down while it plays
                    // is when anybody reaches for an ear at all.
                    //
                    // The tick is here too, rate-limited by `feed_tick` to
                    // once a second: without it the idle clock goes stale for
                    // the whole length of the story, since the top-of-loop
                    // tick never runs while this call is still awaiting.
                    // Only a story the box can name has a place worth keeping.
                    // A root `.TAF` played from the console arrives here too,
                    // as `Source::First`, and `CONTENT_DIRECTORY` then holds
                    // whatever the last real story left behind — so without
                    // this guard a console `taf` would write its chapter into
                    // some other figure's file, and reaching its end would
                    // wipe that figure's place entirely.
                    let identified = matches!(request, REQUEST_CONTENT | REQUEST_CACHE);
                    let stock = request == REQUEST_CONTENT;
                    let dir = CONTENT_DIRECTORY.load(Ordering::Relaxed);
                    let file = CONTENT_FILE.load(Ordering::Relaxed);
                    let from = match critical_section::with(|cs| {
                        core::mem::replace(&mut *PLAY_FROM.borrow_ref_mut(cs), Position::Start)
                    }) {
                        // Taken either way, so a figure's place cannot be
                        // inherited by whatever plays next.
                        from if identified => from,
                        _ => Position::Start,
                    };
                    let mut attend = || {
                        if let Some(event) = take_plate_event(&mut placed) {
                            apply(&mut reducer, card, event, placed.token());
                        }
                        apply_ear_events(&mut reducer, card, placed.token());
                        feed_tick(&mut reducer, card, placed.token(), &mut last_tick_fed);

                        // Free, and the only thing keeping the reducer's idea
                        // of the position true: it is what gets written when
                        // the figure comes off.
                        reducer.note_position(Position::Exact {
                            page: audio::PAGE.load(Ordering::Relaxed),
                        });
                    };
                    // The hardware comes back, so playing again needs no
                    // reboot — which is what makes stopping worth anything.
                    let (outcome, tx, buffer) =
                        audio::play_taf(card, tx, buffer, source, from, &mut attend).await;
                    if let Err(reason) = outcome {
                        esp_println::println!("teddiebox: playback failed — {reason}");
                    }
                    if matches!(outcome, Ok(audio::Finish::Ended)) {
                        if identified {
                            // A finished story is not a paused one. Zero
                            // rather than a deletion: `storage` has no
                            // delete, and page 0 is the header rather than
                            // audio, so "no file" and "page zero" already
                            // mean the same thing.
                            let mut out = [0u8; MAX_POSITION];
                            let len = position::render(0, &mut out);
                            let _ = card.write_position(stock, dir, file, &out[..len]);
                            // And whatever RAM was holding for it, which
                            // would otherwise outrank the card that was just
                            // cleared.
                            forget_place(((dir as u64) << 32) | file as u64);
                        }
                        // The reducer has no other way to learn this: the
                        // only other exit from playing is the figure being
                        // lifted. Fed for every ended story, identified or
                        // not — a console `play` running out must leave the
                        // reducer idle too, not just a story tied to a figure.
                        apply(&mut reducer, card, Event::PlaybackEnded, placed.token());
                    }
                    // Cleared only by the playback the flag names, and
                    // whatever the outcome: a failed announcement that left it
                    // set would be a shutdown nothing could ever reach.
                    if dir == ANNOUNCING_DIRECTORY.load(Ordering::Relaxed)
                        && file == ANNOUNCING_FILE.load(Ordering::Relaxed)
                    {
                        ANNOUNCING.store(false, Ordering::Relaxed);
                    }
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
async fn bring_up(
    radio: &mut net::Radio<'_>,
    tls: Option<&tls::Client>,
    stay_up: bool,
) -> Option<Unavailable> {
    let Some(config) = credentials() else {
        esp_println::println!(
            "teddiebox: net no credentials — type `net ssid <name>` then `net pw <passphrase>`"
        );
        // Not `Refused`: nothing was refused, because nothing was asked. A box
        // with no credentials at all already said `ConfigError` out loud when
        // it failed to read the card, so this stays the general word.
        return Some(Unavailable::Unreachable);
    };

    let seed = net::seed();

    let (mut session, mut link) = match radio.acquire(&config, seed) {
        Ok(pair) => pair,
        Err(e) => {
            esp_println::println!("teddiebox: net could not power the radio — {e:?}");
            return Some(Unavailable::Unreachable);
        }
    };
    esp_println::println!("teddiebox: net associating with {}", config.ssid);

    // Association is mostly waiting on the access point, so the key's one-off
    // setup runs inside it rather than inside the first handshake — once the
    // request is out, which is what the yield is for. See `tls::Client::warm`.
    //
    // Not while audio plays: the setup holds this executor for up to 3.5 s
    // while the radio competes for the CPU (measured), which the decoder
    // cannot ride out. Skipped, the first handshake pays it as it always did.
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

    // The runner has to be polled throughout, not awaited first: it never
    // returns, and nothing else here makes progress without it.
    match select(link.run(), associate).await {
        Either::First(_) => unreachable!("the runner never returns"),
        Either::Second(Err(e)) => {
            // The console keeps the driver's own reason; the reducer gets the
            // one word it can act on, and the two words send a person to
            // different places — the card, or the router.
            let refused = net::refused_credentials(&e);
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
            // Associated, so the passphrase was right; the network is simply
            // not answering. That is the router's end, not the card's.
            return Some(Unavailable::Unreachable);
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
                NET_GET => {
                    match tls {
                        None => {
                            esp_println::println!("teddiebox: tls context unavailable");
                            // A probe waiting on this would otherwise never be
                            // answered, and the figure on the plate would stay
                            // silent for as long as it sat there.
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
                                ruid: requested,
                                token,
                                probe: true,
                            }) => {
                                let wanted = tls::Wanted {
                                    server: &config.server,
                                    ruid: requested.to_be_bytes(),
                                    token: token.as_ref(),
                                    from: None,
                                    etag: None,
                                };
                                let answer = tls::length(tls, &stack, &wanted).await;
                                match answer {
                                    Ok(teddiebox_cloud::Probed::Length(total)) => {
                                        esp_println::println!(
                                            "teddiebox: ask {requested:016X} is {total} bytes"
                                        );
                                        post_answer(
                                            requested,
                                            teddiebox_download::Answer::Length(total),
                                        );
                                    }
                                    // Every remaining case plays what is on the
                                    // card. They are told apart here, where a
                                    // bench can read them, and nowhere else.
                                    Ok(other) => {
                                        esp_println::println!(
                                            "teddiebox: ask {requested:016X} learned nothing — {other:?}"
                                        );
                                        post_answer(requested, teddiebox_download::Answer::Nothing);
                                    }
                                    Err(e) => {
                                        esp_println::println!(
                                            "teddiebox: ask {requested:016X} failed — {e:?}"
                                        );
                                        post_answer(requested, teddiebox_download::Answer::Nothing);
                                    }
                                }
                            }
                            Some(FetchRequest {
                                ruid: requested,
                                token,
                                probe: false,
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

                                // The conversation with the card's owner. Re-asking is
                                // the point of it: the media task only runs
                                // `service_download` between requests, and `attend` —
                                // what it calls while a sound or a story is playing —
                                // does not, so the first ask lands unheard whenever a
                                // figure is placed during a prompt, which is most of
                                // the time.
                                let mut handshake = Handshake::new();
                                let mut step = handshake.requested(Instant::now().as_millis());
                                let settled = loop {
                                    match step {
                                        Step::AskCard => DOWNLOAD_STATE
                                            .store(DOWNLOAD_PREPARING, Ordering::Relaxed),
                                        Step::Wait => {}
                                        settled => break settled,
                                    }
                                    Timer::after(Duration::from_millis(HANDSHAKE_POLL_MS)).await;
                                    // The answer is looked for before the clock is
                                    // read, so one that has already landed is never
                                    // buried under a re-ask.
                                    step = if DOWNLOAD_STATE.load(Ordering::Relaxed)
                                        == DOWNLOAD_PLANNED
                                    {
                                        handshake.card_answered(CardSays::from_offset(
                                            DOWNLOAD_FROM.load(Ordering::Relaxed),
                                        ))
                                    } else {
                                        handshake.polled(Instant::now().as_millis())
                                    };
                                };

                                let from = match settled {
                                    Step::Play => {
                                        esp_println::println!(
                                            "teddiebox: get the card already holds all of it"
                                        );
                                        // Nothing to fetch is still an answer, and it is
                                        // the good one: whoever asked can play now.
                                        fetch_ended(FETCH_COMPLETED);
                                        DOWNLOAD_STATE.store(DOWNLOAD_IDLE, Ordering::Relaxed);
                                        None
                                    }
                                    // **Not a fetch from zero.** That was the old
                                    // fallback, and it truncates the partial file it
                                    // lands on — so a card that was merely too busy to
                                    // answer lost the quarter of an hour it had already
                                    // spent downloading. Saying so and stopping leaves
                                    // what is cached intact for the next attempt.
                                    Step::GiveUp => {
                                        esp_println::println!(
                                            "teddiebox: get the card did not answer in {} s — \
                                             leaving what is cached alone",
                                            teddiebox_download::DEADLINE_MS / 1_000
                                        );
                                        fetch_ended_if_silent(FETCH_UNREACHABLE);
                                        DOWNLOAD_STATE.store(DOWNLOAD_IDLE, Ordering::Relaxed);
                                        None
                                    }
                                    Step::Fetch { from } => Some(from),
                                    Step::Wait | Step::AskCard => {
                                        unreachable!("the loop above breaks on nothing else")
                                    }
                                };

                                if let Some(from) = from {
                                    if from > 0 {
                                        esp_println::println!(
                                            "teddiebox: get resuming from {from}"
                                        );
                                    }
                                    DOWNLOAD_ABORT.store(false, Ordering::Relaxed);
                                    if token.is_some() {
                                        esp_println::println!(
                                            "teddiebox: get sending the tag's token"
                                        );
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
                                        // Not a failure, and not the reducer's
                                        // business: it learned the figure was gone
                                        // before this loop did, and that is what
                                        // told this loop to stop. Saying
                                        // "unreachable" now would have the box
                                        // announce a network fault for a story
                                        // nobody is waiting for.
                                        Err(tls::Error::Abandoned) => esp_println::println!(
                                            "teddiebox: get abandoned — nobody is waiting for it"
                                        ),
                                        Err(e) => {
                                            // The console keeps the real code; the
                                            // reducer gets the one word it can act on.
                                            esp_println::println!("teddiebox: get failed — {e:?}");
                                            fetch_ended(outcome_for(why_unavailable(&e)));
                                        }
                                    }
                                }
                            }
                        },
                    }
                    // An association raised for one story goes down with
                    // it. Asking here rather than at the top of the loop
                    // keeps the radio up for the whole of the fetch,
                    // including the part that happens after the last byte
                    // leaves the socket.
                    if !stay_up {
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
    // It came up. Whatever happened to the fetch after that was reported by
    // whoever was doing it, and is not this function's to name.
    None
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
            // The console asked, and the console is reading the log: the
            // reason is printed inside, and there is no figure waiting to be
            // told anything.
            NET_UP => {
                let _ = bring_up(&mut radio, tls.as_ref(), true).await;
            }
            // `net status` and `net down` are answered inside `bring_up` while
            // it is running. Reaching them here means it is not.
            NET_STATUS | NET_DOWN | NET_TLS => {
                esp_println::println!("teddiebox: net is down")
            }
            // A story to fetch with the radio down. The radio comes up for
            // it and goes down again with it: this box cannot hold a figure
            // reliably while the radio works, and it has no other reason to be
            // associated — so an association is something a download borrows
            // rather than a state the box sits in.
            NET_GET => match critical_section::with(|cs| *FETCH_REQUEST.borrow_ref(cs)) {
                None => {
                    esp_println::println!("teddiebox: get has no request queued — nothing to fetch")
                }
                Some(FetchRequest { ruid, probe, .. }) => {
                    // Attributed before anything can end the fetch, the same as
                    // every other path that ends one.
                    FETCH_ACTIVE_RUID.store(ruid, Ordering::Relaxed);
                    esp_println::println!(
                        "teddiebox: net coming up {}",
                        if probe {
                            "to ask about a figure"
                        } else {
                            "for a story"
                        }
                    );
                    // Put back for the association's own loop to serve: it was
                    // taken out of the request by the read at the top of this
                    // one, and it is the whole reason for associating.
                    NET_REQUEST.store(NET_GET, Ordering::Relaxed);
                    let gave_up = bring_up(&mut radio, tls.as_ref(), false).await;
                    // The pre-arm above is only ever consumed by `bring_up`'s
                    // own inner loop, reached after a successful association —
                    // every early return happens before that loop starts. Left
                    // uncleared here, this exact request would still be
                    // sitting in `NET_REQUEST` on this task's next iteration,
                    // and it would retry the identical fetch forever: on a
                    // network that keeps refusing, that competed with
                    // playback for the executor every failed attempt and
                    // measurably glitched the audio (up to 291 DMA restarts
                    // over 263 s, 2026-09-17).
                    if gave_up.is_some() {
                        NET_REQUEST.store(REQUEST_NONE, Ordering::Relaxed);
                    }
                    // A question that reaches here unanswered is answered now,
                    // whatever went wrong. The reducer holds the figure silent
                    // until it hears back, and a radio that would not come up
                    // is not a reason to refuse a story that is on the card.
                    if probe {
                        if !answer_waiting() {
                            esp_println::println!(
                                "teddiebox: net would not come up to ask — playing what is here"
                            );
                            probe_ended(ruid);
                        }
                    }
                    // `bring_up` answers a fetch it could not start — no
                    // credentials, no association, no lease — by returning
                    // without serving the request. Nobody else will, so
                    // whoever asked is told here rather than left waiting,
                    // and told which of the two faults it was: a figure whose
                    // story cannot be fetched because the passphrase is wrong
                    // should not send anybody to look at a working router.
                    else if DOWNLOAD_STATE.load(Ordering::Relaxed) == DOWNLOAD_IDLE
                        && fetch_ended_if_silent(outcome_for(
                            gave_up.unwrap_or(Unavailable::Unreachable),
                        ))
                    {
                        esp_println::println!("teddiebox: net would not come up for it");
                    }
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

    let mut reader = match nfc::Reader::open(spi, cs, irq, esp_hal::delay::Delay::new()).await {
        Ok(reader) => reader,
        Err(reason) => {
            esp_println::println!("teddiebox: nfc failed — {reason}");
            return;
        }
    };

    let mut presence = Presence::new(ARRIVALS_TO_AGREE, MISSES_TO_LEAVE);
    // Whether polling was on last time round, so switching it on can start
    // from a clean sheet. See where it is used.
    let mut was_polling = false;
    let mut ticks_since_poll: u8 = 0;
    // Consecutive unanswered polls against a figure believed present.
    let mut misses: u16 = 0;
    // Whether the last poll found a figure. Chooses which question the next
    // poll asks first; a wrong guess costs one extra unanswered exchange and
    // corrects itself on the following poll, which `MISSES_TO_LEAVE` absorbs.
    let mut believed_present = false;

    loop {
        // Nothing to do for the rest of this power-on: see `PARKED`.
        if PARKED.load(Ordering::Relaxed) {
            park_task().await;
        }

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

        let polling = PLATE_POLLING.load(Ordering::Relaxed);
        // **Switching polling on forgets what the plate used to hold.**
        // `Presence` reports changes, not states: a figure it already believes
        // is present, fed again, is not an arrival — correctly, because
        // nothing happened. But polling that stops and starts is not nothing
        // happening, and the figure may have been swapped or taken in between,
        // with no poll running to notice.
        //
        // Without this, `plate on` with a figure already sitting there
        // announces nothing at all and its story never starts, while the
        // reader answers perfectly — which is exactly what it looks like from
        // a console, and cost an afternoon on 2026-09-14 being mistaken for a
        // blind reader. The `nfc` command reading the same tag instantly is
        // not a contradiction: it was never the reader that was quiet.
        if polling && !was_polling {
            presence = Presence::new(ARRIVALS_TO_AGREE, MISSES_TO_LEAVE);
            believed_present = false;
            misses = 0;
        }
        was_polling = polling;

        if polling {
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
                // How many polls in a row have found nothing. `MISSES_TO_LEAVE`
                // turns four of these into a departure, and on 2026-09-07 the
                // radio produced 23 false departures in ten minutes — so
                // whether the misses come in ones and twos or in long runs
                // decides whether tolerating more of them is a fix or a
                // plaster.
                //
                // Counted on `seen` alone, deliberately. An earlier version of
                // this gated the count on `believed_present`, which is the
                // *previous* poll's answer and is already false by the second
                // miss — so it could never count past one, which was the whole
                // question. A miss is a poll that saw nothing, whatever the
                // poller expected to see.
                if seen.is_none() {
                    misses = misses.saturating_add(1);
                } else {
                    if misses > 0 {
                        esp_println::println!(
                            "teddiebox: plate answered again after {misses} missed polls"
                        );
                    }
                    misses = 0;
                }
                believed_present = seen.is_some();

                if let Some(event) = presence.feed(seen.map(TagUid)) {
                    // The poller's own view of the plate, which until now was
                    // visible only through whatever the reducer decided to do
                    // about it: a tag lost while nothing was playing left no
                    // trace at all.
                    match event {
                        TagEvent::Arrived(TagUid(uid)) => esp_println::println!(
                            "teddiebox: plate tag arrived {:016X}",
                            TagUid(uid).ruid()
                        ),
                        TagEvent::Left => esp_println::println!(
                            "teddiebox: plate tag left after {misses} missed polls"
                        ),
                    }
                    match event {
                        TagEvent::Arrived(TagUid(uid)) => {
                            // Read the token in the same session that found
                            // the tag: it is only readable while the figure
                            // is on the plate and unlocked, and it is needed
                            // when teddyCloud has to go upstream for the
                            // story.
                            let token = reader.read_token();
                            PLATE_TAG.signal(Seen::Figure { uid, token });
                        }
                        TagEvent::Left => PLATE_TAG.signal(Seen::Nothing),
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

    // Clear the ROM's force-download-boot request. It lives in the RTC domain
    // and survives a reset — that is what makes `dl` work — so leaving it set
    // would send every future reset back into download mode. Clearing it here
    // means the box can only ever be one reset away from running again.
    //
    // Ahead of the OTA check below on purpose: that check can itself reboot
    // (a Revert), and a Revert before this bit is cleared would send the box
    // into download mode instead of the other slot.
    esp_hal::peripherals::LPWR::regs()
        .option1()
        .modify(|_, w| w.force_download_boot().clear_bit());

    // Before anything else — including the stack paint below — because this
    // is the check that catches an image which crashed on its *previous*
    // boot before reaching mark_valid. Nothing above this line has run yet
    // on the attempt that is being judged, so nothing above it can be the
    // thing that failed last time.
    ota::confirm_boot_or_revert();

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

    let mut board = BoardPins::new(p.GPIO45, p.GPIO47);
    let mut gates = Gates::at_reset();

    // Held until something asks the box to stop for good. Deep sleep consumes
    // it, so it is parked in an `Option` the way the other single-use
    // peripherals are rather than being taken at the point of use.
    let mut lpwr = Some(p.LPWR);

    spawner.spawn(heartbeat().unwrap());

    // The LED is on this rail, so it has to come up before anything —
    // including the setup portal below — can paint it.
    board.apply(gates.power(Rail::Peripherals, true));

    // The controller and its timer are bound here, ahead of everything else
    // that used to be the first thing built after the rail, because the
    // setup branch below needs a controller to paint through and never
    // reaches the loop that owns one for every other mode. The channels
    // borrow the timer, so it has to outlive them; `main` never returns,
    // which is exactly long enough regardless of where in it this sits.
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

    // Ears and wake are all active low, so they are read with a pull-up: an
    // unconnected input then reads as "not pressed" rather than floating into
    // phantom presses.
    let up = InputConfig::default().with_pull(Pull::Up);
    let larger = Input::new(p.GPIO20, up);
    let smaller = Input::new(p.GPIO21, up);

    // Spawned above the setup branch, not below it, because setup mode is the
    // one mode that needs it most: an access point beacons for as long as it
    // is up, and this pack has gone flat while still reporting itself healthy.
    // Everything else below is deliberately left until after the branch.
    let mut adc_config = AdcConfig::new();
    let battery = adc_config.enable_pin(p.GPIO9, Attenuation::_11dB);
    let charger = adc_config.enable_pin(p.GPIO8, Attenuation::_11dB);
    spawner.spawn(sense(Adc::new(p.ADC1, adc_config), battery, charger).unwrap());

    // Both ears held through a power-on asks for the setup portal. Read
    // before `net` is spawned — that task is the only other claim on
    // `p.WIFI` — and before the rest of the boot, because setup mode is the
    // *absence* of almost every task below: no decoder, no codec, no NFC, no
    // media loop.
    //
    // **That absence does not pay for the portal's memory, and an earlier
    // version of this comment claimed it did.** What the portal costs is
    // `.bss`, not stack: its buffers are held across `await` points, so they
    // size this task's future, and `esp-hal` gives the stack whatever `.bss`
    // leaves. Not starting the decoder frees no `.bss` at all — see the
    // measurements in `portal.rs`, which are of the linker's own symbols.
    //
    // Active low, so held is low. The ears are the gesture and not a
    // control: once this branch is taken they are never read again.
    if larger.is_low() && smaller.is_low() {
        esp_println::println!("teddiebox: both ears held — setup portal");

        // Nothing drains `INPUT_EVENTS` here: the reducer lives in the media
        // task and that is never spawned. Told now rather than discovered
        // later, so `sense` keeps its readings to the console instead of
        // filling an eight-slot queue and then complaining about it once
        // every two seconds for as long as the portal is up.
        SETUP_MODE.store(true, Ordering::Relaxed);

        // Nothing before this point brings the card up: the rest of the boot
        // does that lazily, inside `media`, on the first command that needs
        // it. Setup mode never reaches `media`, so the same bus and the same
        // rail are brought up here instead, once, so `portal::run` has the
        // card it needs to read and rewrite `CONFIG.TXT`.
        board.apply(gates.power(Rail::Storage, true));
        Timer::after(Duration::from_millis(50)).await;

        // The main loop below is what paints `LED_REQUEST` for every other
        // mode; this branch never reaches it, so painting happens straight
        // onto the controller built above instead. Built once and handed
        // down rather than stored anywhere, because nothing outside this
        // branch and `portal::run` ever needs to ask for a colour again.
        let paint = |state: LedState| {
            if let Some(rgb) = rgb.as_ref() {
                if let Ok(lit) = gates.led(colour_for(state)) {
                    rgb.apply(&lit, board::LED_DUTY);
                }
            }
        };

        // Claimed before the card is even mounted, because the failure path
        // below parks — and parking is an `await`, so anything alive at that
        // point is a field of this task's future and therefore of `.bss`.
        // Asking for the scratch first keeps the 812-byte `Mounted` out of
        // it: by the time the card exists, the only remaining `await` in this
        // branch is the one that has already handed the card away.
        let Some(scratch) = audio::take_scratch_bytes() else {
            esp_println::println!(
                "teddiebox: portal cannot have the decode scratch — it is in use"
            );
            paint(LedState::Error);
            portal::park().await
        };

        // The access point goes up whether or not the card does. A loose or
        // dead card is exactly the box somebody is holding both ears on, and
        // painting it red with no network to join leaves them nowhere to go:
        // the page says what is wrong instead, and only the *save* is
        // refused. The console keeps the specific reason; the page only needs
        // to say there is no card.
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

        // Setup mode runs out of the decoder's scratch. `portal::run`'s
        // future — every socket buffer, the card, and the radio's
        // `StackResources` — is built on the stack here and immediately moved
        // into the 51,712 bytes `audio::SCRATCH` is holding for a decoder
        // this mode never starts, so what stays in *this* task's future is a
        // pointer. That is what keeps the linker's stack region where it was
        // before the portal existed; `portal::place` carries the numbers.
        //
        // The claim above goes through the same gate the decoder's does, so
        // the two cannot both have it: a box that somehow got here with audio
        // running raises no access point and says why, rather than aliasing
        // the buffer the decoder is writing into.
        let Some(portal) = portal::place(
            scratch,
            portal::run(p.WIFI, p.UART0, p.GPIO44, card, seed, paint),
        ) else {
            // `place` has already said what would not fit. Park rather than
            // reset, for the reason `portal::run` parks: the ears are still
            // held, so a reset comes straight back here.
            portal::park().await
        };
        portal.await
    }

    spawner.spawn(net(p.WIFI, p.SHA, p.RSA, p.AES).unwrap());

    // One task per ear, each held by its own pin: the GPIO hardware reports a
    // press whenever this box next gets around to asking, which a 2 ms poll
    // could not do under a decoder using 69% of real time.
    spawner.spawn(ear(Ear::Larger, "larger ear", larger).unwrap());
    spawner.spawn(ear(Ear::Smaller, "smaller ear", smaller).unwrap());
    spawner.spawn(inputs(Input::new(p.GPIO7, up)).unwrap());

    // UART0's receive half. esp-println keeps the transmit half.
    // Let the console drain before UART0 is reconfigured below.
    //
    // Every boot line printed since reset is still queued in UART0's transmit
    // FIFO at this point, and `UartRx::new` reprograms the peripheral out from
    // under it — so the last line or two arrives truncated and spliced into
    // whatever prints next. That is the corruption in front of the command
    // list that every boot has shown for months: `teddieb 0x1teddiebox: dl rb`
    // is `stack painted` being cut off mid-word. At 115200 the queue is a few
    // hundred microseconds, and `drain_console` already waits exactly this way
    // for exactly this reason on the way out of a reboot.
    drain_console();

    let mut console = UartRx::new(p.UART0, UartConfig::default().with_baudrate(115200))
        .expect("UART0 receive")
        .with_rx(p.GPIO44);
    let mut watch = CommandWatch::new();
    // Cleared the moment it is asked for, so a jingle is a start-up event and
    // not something that can happen twice.
    let mut startup_pending = true;
    // Separate from startup_pending: the jingle only needs the codec, but
    // confirming an update needs the card too, and the card mounts lazily
    // on whichever request happens to need it first.
    let mut boot_confirmed = false;
    esp_println::println!(
        "teddiebox: dl rb | t wav taf play <id>[/<id>|<16hex>] stop (loud) | sd | nfc pw slix slixp lock mem <2hex> <2hex> token | net scan ssid <name> pw <pass> up down tls status | get <16hex> | crc <16hex> | stack | cinit cdown cset cclr out spk | pcm <2hex> | batlog <seconds> | slap <2hex> slapt <2hex> | plate on|off | awake on|off | sleep | autosleep on|off | reval"
    );

    // Deliberately not up by `ota::confirm_boot_or_revert()`, where nothing
    // else depends on it: `stack::paint()` fills every unused byte of stack
    // with a pattern, including whatever a print from just before it hadn't
    // finished draining out the UART yet, and that print was the corpse.
    // Moving this below both the paint and the console banner is what keeps
    // it out of that memory — pulling it back up there garbles the line
    // again. `flash::flash()` lends one handle at a time: this takes it and
    // gives it back inside the call.
    //
    // It also sits below the both-ears-held branch above, which diverges
    // with `-> !` and never reaches here: setup mode never loads the
    // identity. Inert today — the portal makes no outbound TLS connection —
    // but it is a real narrowing, and this is the only place it is recorded.
    identity::load();

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
                    // Gate 47 feeds the reader as well as the card, so a box
                    // that polls the plate from boot has to raise it here.
                    // `plate on` does exactly this before setting the flag;
                    // until the default changed, that command was the only
                    // way the reader was ever started, and the rail came with
                    // it. Raised before the spawn rather than in the loop
                    // below, because the task settles for 50 ms and then
                    // talks — which is a race against a rail nobody has
                    // raised, and it reads as "suspect SPI".
                    if PLATE_POLLING.load(Ordering::Relaxed) {
                        board.apply(gates.power(Rail::Storage, true));
                    }
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

    // The LED holds one colour until the reducer decides on another, so this
    // loop only has to repaint often enough that a decision is seen promptly.
    // The waveform is held by the LEDC peripheral either way; the software PWM
    // this replaced woke four hundred times a second and could not keep its
    // own period once anything else wanted the executor.
    const LED_STEP: Duration = Duration::from_millis(20);

    // What the LED is showing now, so a loop pass with no new decision repaints
    // the same colour rather than going dark.
    let mut shown = LedState::Booting;

    // Asked on a wake, before the codec, the card, the radio or the jingle.
    // Speaking costs a codec power-up, the amplifier, a card read and a decode
    // — seconds of near-full-power draw on a pack that has nothing left, every
    // time a child presses an ear, and driving these unprotected cells that
    // far down is the thing the cutoff exists to prevent. Red is milliamps and
    // is still an answer, and "red means plug me in" is learnable.
    //
    // Only on a wake from deep sleep. A cold boot on a flat pack still boots
    // and still speaks: that path is how a box says what is wrong to somebody
    // who just switched it on, and it has not slept, so nothing here knows
    // sleep works on this board.
    if reset_reason(Cpu::ProCpu) == Some(SocResetReason::CoreDeepSleep) {
        if let Some(mv) = pack_says_empty().await {
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
            // A byte that names no state can only come from a bug, and the
            // last colour is a better answer than a dark LED: the box is still
            // running, whatever the byte says.
            if let Some(state) = LedState::from_code(LED_REQUEST.load(Ordering::Relaxed)) {
                shown = state;
            }
            // A dark LED rather than a panic if the rail is ever down here.
            // It is up for every path that reaches this today — the shutdown
            // that lowers it parks without coming back — but this is a loop
            // that runs for the life of the box on a device a child holds.
            if let Ok(lit) = gates.led(colour_for(shown)) {
                rgb.apply(&lit, board::LED_DUTY);
            }
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

        if !boot_confirmed
            && CODEC_READY.load(Ordering::Relaxed)
            && CARD_MOUNTED.load(Ordering::Relaxed)
        {
            boot_confirmed = true;
            ota::mark_valid();
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
            // reports it as under way. Named as well as marked, so only the
            // playback this flag is about can clear it.
            ANNOUNCING_DIRECTORY.store(LANGUAGE.content_directory(), Ordering::Relaxed);
            ANNOUNCING_FILE.store(pending, Ordering::Relaxed);
            ANNOUNCING.store(true, Ordering::Relaxed);
            REQUEST.store(REQUEST_CONTENT, Ordering::Relaxed);
        }

        // The box said it was turning off. Do it, once the sentence has been
        // heard — cutting the announcement short to obey it would be its own
        // kind of broken.
        if SHUTTING_DOWN.load(Ordering::Relaxed) && !ANNOUNCING.load(Ordering::Relaxed) {
            // The last chance anything has to write the card. Asked of the task
            // that owns it, and waited for — but not for long: a pack this
            // close to empty is not worth holding the rails up over, and a lost
            // bookmark is a smaller failure than a box that will not switch
            // off.
            FLUSH_PLACE.store(true, Ordering::Relaxed);
            for _ in 0..20 {
                if !FLUSH_PLACE.load(Ordering::Relaxed) {
                    break;
                }
                Timer::after(Duration::from_millis(25)).await;
            }
            // Which authority decided is said by `perform`, where it is known.
            // This line used to name the pack unconditionally, and an idle park
            // then reported a flat battery that was nothing of the kind.
            esp_println::println!("teddiebox: going dark");
            go_dark(&mut board, &mut gates, rgb.as_ref()).await;
            // Deep sleep is what this was always meant to be. A park keeps the
            // executor running with the PLLs up — tens of milliamps — so the
            // cutoff that exists to stop these unprotected cells being driven
            // into reversal was removing the load a child can see and then
            // draining the pack anyway.
            //
            // The default since the wake was proven on hardware. `autosleep
            // off` is how a bench asks for a box that cannot disappear.
            if AUTO_SLEEP.load(Ordering::Relaxed) {
                if let Err(reason) = sleep_now(&mut lpwr).await {
                    esp_println::println!("teddiebox: sleep not armed — {reason}, parking instead");
                }
            }
            // Everything a child can see or hear is now off, and every task
            // that was still working on the box's behalf stops here. Reached
            // when the ending is a park, and when a sleep would not arm: a box
            // that drains is recoverable by charging, and a box asleep with
            // nothing able to wake it is not, so this is the safe side to fall
            // on.
            PARKED.store(true, Ordering::Relaxed);
            park_task().await;
        }

        let mut buf = [0u8; 16];
        if let Ok(n) = console.read_buffered(&mut buf) {
            let command = buf[..n].iter().find_map(|&b| watch.feed(b));

            // A release image answers `dl` and nothing else. One gate around
            // the whole dispatch rather than one per command: a command added
            // later is gated by having been added, which is the opposite of
            // what a per-arm list does, and `BENCH` being a constant lets the
            // optimiser drop every arm below rather than only their bodies.
            if !BENCH {
                match command {
                    Some(Command::DownloadMode) => {
                        quieten_codec().await;
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
                Some(Command::HeadphoneStatus) => {
                    HEADPHONE_REPORT.store(true, Ordering::Relaxed);
                }
                // Both halves, always: the static the codec bring-up reads,
                // and the event that moves the routing and the ladder
                // together. Setting the static alone would mute the speaker
                // and leave the level on the other output's ladder.
                Some(Command::Headphones(on)) => {
                    HEADPHONES_IN.store(on, Ordering::Relaxed);
                    esp_println::println!(
                        "teddiebox: headphones forced {}",
                        if on { "in" } else { "out" }
                    );
                    if INPUT_EVENTS.try_send(Event::Headphones(on)).is_err() {
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
                    NFC_REQUEST.store(NFC_READ_TOKEN, Ordering::Relaxed);
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
                // Manual, and manual only. The automatic path stays a park
                // until sleep current and the state of the gate pins have been
                // measured, and those two numbers are taken with a meter and a
                // box that sleeps when it is told to — not one that decides to
                // mid-session.
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
                Some(Command::BatteryLog { seconds }) => {
                    BATLOG_EVERY.store(seconds, Ordering::Relaxed);
                    if seconds == 0 {
                        esp_println::println!("teddiebox: batlog off");
                    } else {
                        esp_println::println!("teddiebox: batlog every {seconds} s");
                        esp_println::println!("batlog,ms,raw,mv,playing,charger_raw");
                    }
                }
                Some(Command::SlapTimeLimit { limit }) => {
                    SLAP_TIME_LIMIT.store(limit, Ordering::Relaxed);
                }
                Some(Command::SlapThreshold { threshold }) => {
                    SLAP_THRESHOLD.store(threshold, Ordering::Relaxed);
                }
                Some(Command::Stop) => {
                    audio::STOP.store(true, Ordering::Relaxed);
                }
                // Setup mode's console acts on this; this one does not. Here
                // the card is a file away on a laptop, and the access point it
                // changes is not up — so the command has nothing to fix and
                // one more way to write the card is one more way to get it
                // wrong.
                Some(Command::SetupPassword(_)) => esp_println::println!(
                    "teddiebox: setup pw only works in setup mode — hold both ears at switch-on"
                ),
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

        Timer::after(LED_STEP).await;
    }
}
