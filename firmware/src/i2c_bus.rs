//! The task that owns the shared I2C bus: it brings up the codec, detects
//! slaps on the accelerometer, and carries out the codec requests (volume,
//! speaker, output stages, register overrides) that other tasks post here.

use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, AtomicI8, AtomicU8, Ordering};

use critical_section::Mutex as CsMutex;
use embassy_futures::select::select;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use esp_hal::gpio::{Level, Output};
use esp_hal::i2c::master::I2c;
use lis3dh::{clamped_threshold, regs as lis, ClickAxes, ClickAxis, ClickConfig, Lis3dh};
use teddiebox_board as board;
use teddiebox_core::i2c as bus;
use teddiebox_core::{db_for, Event, Output as AudioOutput, Volume, MAX_VOLUME};
use tlv320dac3100::Tlv320Dac3100;

use crate::{inputs, park_task, PARKED};

/// Brings up the codec, then finds the accelerometer, runs its click
/// detection, and sends [`Event::Slap`] when a click is on the axis
/// `board::side_for_click` maps to a side. Also handles codec requests from
/// other tasks, since this task owns the shared I2C bus.
///
/// Both accelerometer addresses are tried, because 0x18 is shared with the
/// audio codec, which also answers there.
#[embassy_executor::task]
pub(crate) async fn i2c_bus(i2c: I2c<'static, esp_hal::Blocking>, mut reset: Output<'static>) {
    // Reset the codec first: it may still be running, half-configured, from
    // before the last reboot.
    let hold = board::dac_reset(true);
    let run = board::dac_reset(false);
    reset.set_level(if hold.high { Level::High } else { Level::Low });
    Timer::after(Duration::from_millis(10)).await;
    reset.set_level(if run.high { Level::High } else { Level::Low });
    Timer::after(Duration::from_millis(10)).await;

    let mut bus = i2c;

    // The codec first: it is configured once, then the bus goes to the
    // accelerometer, which uses it continuously.
    let mut dac = Tlv320Dac3100::new(bus, tlv320dac3100::DEFAULT_ADDRESS);
    let mut dac_delay = esp_hal::delay::Delay::new();
    match dac
        .reset()
        .and_then(|()| codec_bring_up(&mut dac, &mut dac_delay))
    {
        Ok(()) => {
            esp_println::println!("teddiebox: codec configured");
            CODEC_READY.store(true, Ordering::Relaxed);

            // The start-up volume, matching the reducer's starting step (see
            // `BOOT_VOLUME_DB`).
            if dac.set_volume_db(BOOT_VOLUME_DB).is_err() {
                esp_println::println!("teddiebox: codec volume not set");
            }

            // What actually powered up, not what was requested. Read after
            // `init` has waited for the drivers' ramp; before that, HPL reads
            // as off for 304 ms.
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
    // Starts at the limit, so the first pass reads the headset register at
    // once: a box that boots with headphones in should know within the first
    // pass, not after ~600 ms.
    let mut since_detect: u32 = HEADSET_DETECT_EVERY;
    // Starts matching `HEADPHONES_IN`, so booting with nothing plugged in
    // sends no event. Kept here rather than read from the static, because
    // `hp 1` sets the static, and a forced routing must not be undone by
    // polls that read an unchanged register.
    let mut last_detect = false;
    // Only logged when it changes, not on every poll, so a failed codec does
    // not flood the console.
    let mut detect_failed = false;
    // Also only logged once: with no card mounted nothing reads
    // `INPUT_EVENTS`, so the send below would fail on every poll.
    let mut detect_dropped = false;
    // `armed_threshold` is the last value a write was *attempted* with;
    // `armed` is only set by a successful write. The re-arm below checks
    // `armed`, not just a changed threshold, so a failed write is retried.
    //
    // `confirmed_threshold` is the last value a write actually succeeded
    // with, for printing only. Only set on success, because the click print
    // shows it as the value the chip is using.
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

    match accel.enable_click(ClickConfig {
        // Y only; see `SLAP_AXES` and `board::side_for_click`.
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
            // Reported once; the loop below retries quietly until the write
            // succeeds or a different threshold is set.
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
                // `detect` is register 67: D7 means detection is on, and D6-D5
                // show the last headset type, even after it is removed.
                // `status` is register 46, whose D4 shows whether a plug is in
                // now; this is what the box uses. The routing is the box's own
                // state, which `hp 1` can force.
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
        if output == OUTPUT_UP && OUTPUT_IS_UP.load(Ordering::Relaxed) {
            continue;
        }
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
                Ok(()) => {
                    OUTPUT_IS_UP.store(up, Ordering::Relaxed);
                    esp_println::println!(
                        "teddiebox: codec output {}{}",
                        if up { "up" } else { "down" },
                        if up && !speaker { " (headphones)" } else { "" }
                    )
                }
                Err(_) => {
                    // Counted as down whichever way it failed, so the next
                    // request reruns the whole power-up: a stray click is
                    // better than a speaker left muted until a restart.
                    OUTPUT_IS_UP.store(false, Ordering::Relaxed);
                    esp_println::println!("teddiebox: codec output would not change")
                }
            }
            let bus = dac.release();
            accel = Lis3dh::new(bus, address);
            continue;
        }
        if CODEC_POWER_DOWN.swap(false, Ordering::Relaxed) {
            let bus = accel.release();
            let mut dac = Tlv320Dac3100::new(bus, tlv320dac3100::DEFAULT_ADDRESS);
            match dac.power_down(&mut dac_delay) {
                Ok(()) => {
                    OUTPUT_IS_UP.store(false, Ordering::Relaxed);
                    esp_println::println!("teddiebox: codec powered down")
                }
                Err(_) => esp_println::println!("teddiebox: codec would not power down"),
            }
            let bus = dac.release();
            accel = Lis3dh::new(bus, address);
            continue;
        }
        if CODEC_REINIT.swap(false, Ordering::Relaxed) {
            // Power down first, so every run is a real start-up.
            let bus = accel.release();
            let mut dac = Tlv320Dac3100::new(bus, tlv320dac3100::DEFAULT_ADDRESS);
            let _ = dac.power_down(&mut dac_delay);
            OUTPUT_IS_UP.store(false, Ordering::Relaxed);
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
            // The accelerometer holds the bus, so take it back for the codec.
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
            // Only the reset follows a shutdown.
            return;
        }
        match accel.take_click() {
            Ok(Some(click)) => {
                // Always printed: clicks are rare, and this is used to
                // calibrate. `raw` is included because the datasheet is unclear
                // about CLICK_SRC's bits. With more than one axis bit set,
                // `take_click` picks X, then Y, then Z, so MULTI-AXIS is
                // printed to show that others were dropped.
                let axes_set = (click.raw & 0b111).count_ones();
                // Prints `confirmed_threshold`, the value the chip is using,
                // not `armed_threshold`, which may have failed to apply.
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
                        // Printed, because during calibration the rebounds
                        // show whether the window and threshold are right.
                        esp_println::println!("teddiebox: slap ignored, within refractory");
                    } else {
                        slap_refractory = SLAP_REFRACTORY_POLLS;
                        if inputs::INPUT_EVENTS.try_send(Event::Slap(side)).is_err() {
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
            // A new value gets its own failure line even if the last one was
            // already reported, so it does not look ignored.
            let threshold_changed = wanted != armed_threshold || wanted_limit != armed_limit;
            armed_threshold = wanted;
            armed_limit = wanted_limit;
            // `armed` is only set when the write succeeds, so the success line
            // is true and a failed write is retried on the next pass.
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
                    // Logged once, not on every 200 ms retry, so it does not
                    // bury the click prints.
                    if !failure_reported || threshold_changed {
                        esp_println::println!(
                            "teddiebox: slap threshold {wanted} NOT applied, LIS3DH write failed"
                        );
                        failure_reported = true;
                    }
                }
            }
        }

        since_detect += 1;
        if since_detect >= HEADSET_DETECT_EVERY {
            since_detect = 0;
            // Give the bus back to the accelerometer before acting on the
            // result, so a full input queue cannot leave the codec holding it.
            let bus = accel.release();
            let mut dac = Tlv320Dac3100::new(bus, tlv320dac3100::DEFAULT_ADDRESS);
            let reading = dac.headphones_connected();
            let bus = dac.release();
            accel = Lis3dh::new(bus, address);
            match reading {
                Ok(now) if now != last_detect => {
                    detect_failed = false;
                    // `HEADPHONES_IN`, which the codec bring-up reads, is
                    // stored at once. `last_detect` only changes once the
                    // event is sent, so a failed send is retried on the next
                    // poll instead of leaving the reducer out of step.
                    HEADPHONES_IN.store(now, Ordering::Relaxed);
                    if inputs::INPUT_EVENTS
                        .try_send(Event::Headphones(now))
                        .is_err()
                    {
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

        select(
            Timer::after(Duration::from_millis(ACCEL_POLL_MS)),
            CODEC_WAKE.wait(),
        )
        .await;
    }
}

/// Scans the I2C bus once and names what answers.
///
/// Every device here is behind power gate 2, so run this after that rail is
/// on.
pub(crate) fn scan_i2c(i2c: &mut I2c<'_, esp_hal::Blocking>) {
    esp_println::println!("teddiebox: scanning I2C");
    let mut found = 0;

    for address in bus::FIRST_ADDRESS..=bus::LAST_ADDRESS {
        // A zero-length write only addresses the device. An acknowledgement
        // means it is present; this cannot tell "absent" from a bus fault.
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

/// Applies the start-up sequence with any console overrides in place.
///
/// Used both at boot and by the console `cinit` command, so re-running it
/// from the console does exactly what the next start will do.
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
/// Short, because this loop also handles shutdown and codec requests, and
/// those should not wait long.
const ACCEL_POLL_MS: u64 = 200;

/// `CLICK_THS`, settable from the console without a reflash.
///
/// One step is 62 mg at the +/-8 g range `init` selects, so 40 is about
/// 2.5 g. At 40, eight slaps (four on each side) were all caught with the
/// right direction, but about one knock from handling per session also counts
/// as a slap. At 48, handling was ignored, but one slap was missed and another
/// read as the wrong side. 40 was chosen so the gesture works reliably.
pub(crate) static SLAP_THRESHOLD: AtomicU8 = AtomicU8::new(40);
/// How many polls to ignore clicks for after a slap, at `ACCEL_POLL_MS` each:
/// two is 400 ms.
///
/// One slap is not one click. The box rocks back after the hit, and the
/// rebound also crosses the threshold with the OPPOSITE sign, so it reads as
/// a slap on the other side. Without this window, five slaps on one side gave
/// two correct skips and one backwards one.
///
/// The click is still read and cleared during the window; only the action is
/// skipped. Leaving it unread would keep a stale click for the next slap.
const SLAP_REFRACTORY_POLLS: u8 = 2;

/// Which axes the click detection watches: Y only. Upright, gravity is on X,
/// so X is vertical and a side slap does not show on it; leaving X off also
/// stops setting the box down from counting as a slap.
const SLAP_AXES: ClickAxes = ClickAxes {
    x: false,
    y: true,
    z: false,
};
/// `TIME_LIMIT`, in sample periods: 4 is 10 ms at the 400 Hz `init` sets.
pub(crate) static SLAP_TIME_LIMIT: AtomicU8 = AtomicU8::new(4);

/// How loud the box plays until an ear says otherwise.
///
/// The level of the step `VolumeModel` starts on, so the codec and the reducer
/// agree on the starting volume. Otherwise the first ear press would step from
/// a level that is not on the volume scale. See `teddiebox_core::db_for` for
/// how the scale is set.
const BOOT_VOLUME_DB: i8 = db_for(AudioOutput::Speaker, Volume(MAX_VOLUME / 2));

/// The level the codec should be playing at, in whole dB, or [`NO_VOLUME`].
///
/// The codec is driven by the task that shares its I2C bus with the
/// accelerometer, while the reducer runs in the media task, so the request
/// passes through here, like `OUTPUT_REQUEST` and `SPEAKER_REQUEST`. It is in
/// dB, the codec's unit; volume steps are the reducer's concern.
pub(crate) static VOLUME_REQUEST: AtomicI8 = AtomicI8::new(NO_VOLUME);

/// No level is waiting. Outside the codec's -63.5..=+24 dB range, so it can
/// never collide with a real request.
const NO_VOLUME: i8 = i8::MIN;

/// Set once the codec is configured and would be heard if it were driven.
///
/// The start-up jingle waits for this. `init` waits 400 ms for the output
/// drivers to ramp up, and a jingle started before then loses its start.
pub(crate) static CODEC_READY: AtomicBool = AtomicBool::new(false);

/// Asked for before the rails go down, answered when the codec is quiet.
///
/// The class-D amplifier stays powered during a session, and cutting its
/// supply makes the speaker click. The codec can power down cleanly, but only
/// over I2C, which `i2c_bus` owns, so this is a request rather than a call.
static CODEC_SHUTDOWN: AtomicBool = AtomicBool::new(false);
static CODEC_QUIET: AtomicBool = AtomicBool::new(false);

/// Ask the I2C bus task to take the codec down and bring it back up.
pub(crate) static CODEC_REINIT: AtomicBool = AtomicBool::new(false);

/// Ask the I2C bus task to run the codec's power-down, and nothing else.
pub(crate) static CODEC_POWER_DOWN: AtomicBool = AtomicBool::new(false);

/// What is plugged into the headphone jack, as far as the box knows.
///
/// **The only place the firmware records this.** The detect poll in `i2c_bus`
/// writes it, `hp 1` / `hp 0` override it, and the codec bring-up reads it.
/// `Action::SetOutput` does not write it; it only drives `SPEAKER_REQUEST`, so
/// there are never two copies that could disagree.
pub(crate) static HEADPHONES_IN: AtomicBool = AtomicBool::new(false);

/// Ask the I2C bus task to read and print the headset-detect registers. A
/// request, because the codec sits on the bus that task owns.
pub(crate) static HEADPHONE_REPORT: AtomicBool = AtomicBool::new(false);

/// Full passes of the `i2c_bus` loop between headset-detect reads.
///
/// Each pass waits `ACCEL_POLL_MS`, so three passes is about 600 ms; plugging
/// in takes effect after that plus the codec's 128 ms debounce. Counted in
/// full passes, not iterations: the request handlers `continue`, which skips
/// the wait.
const HEADSET_DETECT_EVERY: u32 = 3;

/// A pending speaker change: 0 nothing, 1 mute, 2 unmute, 3 resume.
pub(crate) static SPEAKER_REQUEST: AtomicU8 = AtomicU8::new(0);
pub(crate) const SPEAKER_MUTE: u8 = 1;
pub(crate) const SPEAKER_UNMUTE: u8 = 2;
/// Power the class-D amplifier if it is not already up, then unmute it.
///
/// Unlike `SPEAKER_UNMUTE`, which only clears the mute bit. A story started
/// with headphones in never powered the amplifier (powering it clicks), so
/// unplugging must power it first. Kept separate so the console `spk 1`
/// command can still test unmuting on its own.
pub(crate) const SPEAKER_RESUME: u8 = 3;

/// A pending output power change: 0 nothing, 1 down, 2 up.
///
/// Only written through [`request_output`], which also wakes the task that
/// acts on it.
static OUTPUT_REQUEST: AtomicU8 = AtomicU8::new(0);
pub(crate) const OUTPUT_DOWN: u8 = 1;
pub(crate) const OUTPUT_UP: u8 = 2;

/// Whether the codec's output is powered, as last set by this task.
///
/// Lets the media task wait for the output before playing a cue, and keeps
/// a second power-up off a powered codec: rerunning the sequence clicks.
pub(crate) static OUTPUT_IS_UP: AtomicBool = AtomicBool::new(false);

/// Ends the `i2c_bus` task's nap early.
///
/// Without it, starting a story's output waited up to 132 ms (measured) for
/// the nap to end.
static CODEC_WAKE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Asks for the codec's output to be powered up or down, now.
pub(crate) fn request_output(request: u8) {
    OUTPUT_REQUEST.store(request, Ordering::Relaxed);
    CODEC_WAKE.signal(());
}

/// Register overrides applied to the codec's start-up sequence.
///
/// The table and its rules live in `tlv320dac3100`, where they are tested;
/// this is only the lock around it.
static CODEC_OVERRIDES: CsMutex<RefCell<tlv320dac3100::Overrides>> =
    CsMutex::new(RefCell::new(tlv320dac3100::Overrides::new()));

pub(crate) fn codec_override_set(page: u8, register: u8, value: u8) -> bool {
    critical_section::with(|cs| {
        CODEC_OVERRIDES
            .borrow_ref_mut(cs)
            .set(page, register, value)
    })
}

pub(crate) fn codec_overrides_clear() {
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
/// A click is better than a box that will not reboot, so `rb` must work even
/// when the codec does not answer.
const CODEC_SHUTDOWN_TIMEOUT_MS: u64 = 800;

/// Asks the codec to go quiet, and waits until it has or the wait runs out.
pub(crate) async fn quieten_codec() {
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
