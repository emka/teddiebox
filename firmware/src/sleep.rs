//! Deep sleep, and how to wake up from it.
//!
//! Only the hardware steps are here. When to sleep is decided in
//! `teddiebox_core`, where it is tested. This file cannot be tested on the
//! host, so it is kept small.

use esp_hal::gpio::{Event as GpioEvent, Input, WakeupConfig};
use esp_hal::peripherals::LPWR;
use esp_hal::rtc_cntl::sleep::{LowPower, RtcSleepConfig};

/// Arms the ear/wake line as the only wake source.
///
/// **Only one wake level.** Deep sleep powers down the normal GPIO block, so
/// the wake pin needs the *low-power* path (per esp-hal's wakeup module). On
/// this chip that path uses one level for all its pins. A second source of
/// the opposite level would need a mode that keeps more of the chip powered,
/// using the current sleep is meant to save. The charger has the opposite
/// level, so it is not a wake source.
///
/// **Both calls are needed.** `listen` sets the wake *condition*, and the
/// wakeup config sets the *path* that works during sleep.
///
/// The line must be released first: if the pin is already at its wake level,
/// the chip wakes immediately. A held ear keeps the line low, so this returns
/// an error rather than waiting; the caller does the waiting.
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
/// The caller must have silenced the codec, released the rails, and armed a
/// wake source with [`arm`]: `sleep_deep` *panics* without one, leaving a box
/// that never wakes. The wake line's pull-up keeps the pin away from its wake
/// level during sleep; without it the pin would float and wake the chip at
/// once.
pub fn enter(lpwr: LPWR<'static>) -> ! {
    LowPower::new(lpwr).sleep_deep(RtcSleepConfig::deep())
}
