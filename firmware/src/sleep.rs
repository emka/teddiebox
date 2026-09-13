//! Deep sleep, and the two rules that make it wakeable.
//!
//! Everything here is register behaviour rather than policy: what this chip
//! needs in order to stop, and what it needs in order to start again. When to
//! stop, and what to say first, is `teddiebox_core`'s business and is tested
//! there. Nothing in this file can be exercised on a host, so it is kept to
//! the smallest surface the job allows.

use esp_hal::gpio::{Event as GpioEvent, Input, WakeupConfig};
use esp_hal::peripherals::LPWR;
use esp_hal::rtc_cntl::sleep::{LowPower, RtcSleepConfig};

/// Arms the ear/wake line as the only wake source.
///
/// **One level group on purpose.** esp-hal's own wakeup module is explicit
/// that a pad which continues to work while the high-performance GPIO
/// peripheral is powered down needs a *low-power* path, and that deep sleep
/// always powers that peripheral down. On this chip the low-power group takes
/// one level for the whole pad mask, so a second wake source of the opposite
/// polarity would fall to a path that keeps the low-power domain alive — which
/// is the current this exists to save. The charger is exactly that opposite
/// polarity, which is why it is not a wake source.
///
/// **Both calls are needed.** `listen` sets the wake *condition* — a pin that
/// does not listen is not a wakeup source at all — and the wakeup config sets
/// the *path* that survives the sleep. Either alone does nothing.
///
/// The line must be released first. A level-triggered wake on a pad already at
/// its wake level ends the sleep the instant it begins, and an ear holds this
/// line down for as long as it is held, so the press that asked for sleep has
/// to be over before this is called. Refused rather than waited out here,
/// because the waiting belongs to the caller that can still print.
pub fn arm(wake: &mut Input<'static>) -> Result<(), &'static str> {
    if wake.is_low() {
        return Err("the wake line is still held down");
    }
    wake.listen(GpioEvent::LowLevel);
    wake.apply_wakeup_config(&WakeupConfig::default().with_low_power_path(true))
        .map_err(|_| "the wake pad has no low-power path")
}

/// Enters deep sleep and does not return.
///
/// The caller must have quietened the codec and released the rails, and must
/// have armed a wake source with [`arm`]: `sleep_deep` *panics* when no source
/// is enabled, and a panic here leaves a box that never wakes and says nothing
/// about why. The pull-up the wake line is configured with is what holds the
/// pad away from its wake level through the sleep; sleep adds no resistor of
/// its own, and a floating pad wakes the chip immediately and every time.
pub fn enter(lpwr: LPWR<'static>) -> ! {
    LowPower::new(lpwr).sleep_deep(RtcSleepConfig::deep())
}
