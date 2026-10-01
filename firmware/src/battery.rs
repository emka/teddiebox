//! The battery and charger voltages: the task that samples them, and the
//! readings other tasks consult before doing something the battery may not
//! survive.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};

use embassy_time::{Duration, Instant, Timer};
use esp_hal::analog::adc::Adc;
use teddiebox_core::power;
use teddiebox_core::{BatteryConfig, Event};

use crate::inputs::INPUT_EVENTS;
use crate::{park_task, BENCH, PARKED, PLAYING};

/// Set when the box is in setup mode (the portal) instead of normal
/// operation.
///
/// Read by [`sense`], the one task that also runs in setup mode. In setup
/// mode nothing else uses its readings, so they only go to the console.
pub(crate) static SETUP_MODE: AtomicBool = AtomicBool::new(false);

/// Reports the battery and charger voltages.
///
/// The conversion is calibrated at one point only (see
/// `teddiebox_core::power`), and the ESP32-S3's ADC is not linear, so treat
/// the values as approximate.
#[embassy_executor::task]
pub(crate) async fn sense(
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
    // Often enough that the LED changes soon after the charger is unplugged.
    const SAMPLE_EVERY: Duration = Duration::from_secs(2);
    // Only printed on development builds.
    const PRINT_EVERY: u8 = 5;
    let mut since_printed = 0u8;
    let mut batlog_ticks: u8 = 0;

    loop {
        // Nothing to do for the rest of this power-on: see `PARKED`.
        if PARKED.load(Ordering::Relaxed) {
            park_task().await;
        }

        // Raw counts as well as millivolts, since the conversion is based on
        // assumptions that could be wrong.
        let pack_raw = adc.read_blocking(&mut battery);
        let charger_raw = adc.read_blocking(&mut charger);
        let pack_mv = power::battery_mv(pack_raw);
        let charger_mv = power::charger_mv(charger_raw);

        // Published for boot, which must decide whether the battery is good
        // enough before powering anything up. The counter shows whether a
        // reading is new.
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
                    esp_println::println!("teddiebox: battery event dropped — the queue is full");
                }
            }
        }

        since_printed += 1;
        if BENCH && since_printed >= PRINT_EVERY {
            since_printed = 0;
            esp_println::println!(
                "teddiebox: battery {pack_mv} mV (raw {pack_raw}), charger {charger_mv} mV (raw {charger_raw})"
            );
        }

        // One CSV line with raw values. The header is printed when the log
        // starts, so a capture can go straight into a plotting tool.
        let every = BATLOG_EVERY.load(Ordering::Relaxed);
        if every > 0 {
            batlog_ticks += 1;
            // Compared in u64. An interval that is not a multiple of the 2 s
            // sample period rounds up to the next sample.
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

/// The most recent battery reading, in millivolts, and how many have been taken.
///
/// Zero samples means no reading yet, which is not the same as a flat battery.
static PACK_MV: AtomicU32 = AtomicU32::new(0);
static PACK_SAMPLES: AtomicU32 = AtomicU32::new(0);

/// Whether the battery is too low to spend radio time nobody asked for.
///
/// No reading yet counts as not low. The sense task samples every two seconds,
/// so there is normally a reading long before the jingle ends.
pub(crate) fn pack_too_low_to_prime() -> bool {
    PACK_SAMPLES.load(Ordering::Relaxed) > 0
        && PACK_MV.load(Ordering::Relaxed) < u32::from(BatteryConfig::default().low_mv)
}

/// Waits for fresh battery readings and answers whether they agree it is empty.
///
/// **Two readings, not the four of `readings_to_agree`.** Agreement guards
/// against one implausible sample (a reading of 9453 mV from three NiMH cells
/// has been seen), and two consecutive readings are enough for that. Four
/// would keep the box dark for eight seconds while a child holds an ear, and
/// a wrong answer here only costs a return to sleep.
///
/// `None` as soon as a reading is at or above the cutoff, and `None` if no
/// reading arrives: without an answer, booting normally is the safe choice.
pub(crate) async fn pack_says_empty() -> Option<u32> {
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

/// Seconds between `batlog` lines, or zero for off.
pub(crate) static BATLOG_EVERY: AtomicU8 = AtomicU8::new(0);
