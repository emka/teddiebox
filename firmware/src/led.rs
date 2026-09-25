//! The RGB LED, driven by the LEDC peripheral.
//!
//! Hardware PWM keeps the LED steady without any CPU work, so busy tasks
//! (like audio) cannot make it flicker. The main loop only sets the
//! brightness.
//!
//! [`teddiebox_board`] decides which channels are lit; this turns that into
//! duty cycles.

use esp_hal::gpio::interconnect::PeripheralOutput;
use esp_hal::ledc::channel::{self, Channel, ChannelIFace};
use esp_hal::ledc::timer::{self, TimerIFace};
use esp_hal::ledc::{LSGlobalClkSource, Ledc, LowSpeed};
use esp_hal::time::Rate;
use teddiebox_board::{self as board, PinLevel};

/// Fast enough that no eye or camera sees flicker, and well within the
/// peripheral's limits at eight-bit resolution.
pub const PWM_HZ: u32 = 1_000;

/// The three channels that make up the LED.
pub struct Rgb<'a> {
    red: Channel<'a, LowSpeed>,
    green: Channel<'a, LowSpeed>,
    blue: Channel<'a, LowSpeed>,
}

/// Prepares the LEDC peripheral and its timer.
///
/// Separate from [`Rgb`] because the channels borrow the timer, so it must
/// outlive them. In practice both live in `main`, which never returns.
pub fn controller(ledc: esp_hal::peripherals::LEDC<'_>) -> Ledc<'_> {
    let mut ledc = Ledc::new(ledc);
    ledc.set_global_slow_clock(LSGlobalClkSource::APBClk);
    ledc
}

/// Configures the timer the three channels share.
pub fn timer<'a>(ledc: &'a Ledc<'a>) -> Result<timer::Timer<'a, LowSpeed>, ()> {
    let mut timer = ledc.timer::<LowSpeed>(timer::Number::Timer0);
    timer
        .configure(timer::config::Config {
            duty: timer::config::Duty::Duty8Bit,
            clock_source: timer::LSClockSource::APBClk,
            frequency: Rate::from_hz(PWM_HZ),
        })
        .map_err(|_| ())?;
    Ok(timer)
}

impl<'a> Rgb<'a> {
    /// Claims the three LED pins as PWM outputs.
    pub fn new(
        ledc: &'a Ledc<'a>,
        timer: &'a dyn TimerIFace<LowSpeed>,
        red: impl PeripheralOutput<'a>,
        green: impl PeripheralOutput<'a>,
        blue: impl PeripheralOutput<'a>,
    ) -> Result<Self, ()> {
        let mut rgb = Self {
            red: ledc.channel(channel::Number::Channel0, red),
            green: ledc.channel(channel::Number::Channel1, green),
            blue: ledc.channel(channel::Number::Channel2, blue),
        };

        for channel in [&mut rgb.red, &mut rgb.green, &mut rgb.blue] {
            channel
                .configure(channel::config::Config {
                    timer,
                    duty_pct: off_duty(),
                    drive_mode: esp_hal::gpio::DriveMode::PushPull,
                })
                .map_err(|_| ())?;
        }

        Ok(rgb)
    }

    /// Lights the channels `levels` says are on, at `brightness` out of 255.
    ///
    /// `levels` comes from `Gates::led`, which already applies
    /// [`board::LED_ACTIVE_HIGH`], so a channel is lit when its level equals
    /// the active level.
    pub fn apply(&self, levels: &[PinLevel; 3], brightness: u8) {
        for level in levels {
            let lit = level.high == board::LED_ACTIVE_HIGH;
            let duty = if lit { percent(brightness) } else { off_duty() };
            let channel: &Channel<'a, LowSpeed> = match level.gpio {
                board::LED_RED => &self.red,
                board::LED_GREEN => &self.green,
                board::LED_BLUE => &self.blue,
                _ => continue,
            };
            // A failed update only means a wrong brightness until the next
            // one, so the error is ignored.
            let _ = channel.set_duty(duty);
        }
    }
}

/// The duty that leaves an LED dark.
///
/// Not always zero: with active-low wiring the pin must be high to be off,
/// which is full duty. Decided by the same polarity constant as everything
/// else.
const fn off_duty() -> u8 {
    if board::LED_ACTIVE_HIGH {
        0
    } else {
        100
    }
}

/// Converts a 0-255 brightness into the percentage LEDC wants, inverting it
/// when the LED is wired active low.
fn percent(brightness: u8) -> u8 {
    let pct = (brightness as u32 * 100 / 255) as u8;
    if board::LED_ACTIVE_HIGH {
        pct
    } else {
        100 - pct
    }
}
