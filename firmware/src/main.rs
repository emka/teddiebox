#![no_std]
#![no_main]

mod pins;

use embassy_executor::Spawner;
use embassy_time::{Duration, Instant, Timer};
use esp_backtrace as _;
use esp_hal::analog::adc::{Adc, AdcConfig, Attenuation};
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::timer::timg::TimerGroup;
use esp_hal::uart::{Config as UartConfig, UartRx};
use lis3dh::{regs as lis, Lis3dh};
use teddiebox_core::board::{self, Colour, Gates, Rail};
use teddiebox_core::console::CommandWatch;
use teddiebox_core::i2c as bus;
use teddiebox_core::input::{self, Debounced, Edge};
use teddiebox_core::power::{self, PackState};
use tlv320dac3100::Tlv320Dac3100;

use crate::pins::BoardPins;

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
fn reboot_to_download(board: &mut BoardPins, gates: &mut Gates) -> ! {
    esp_println::println!("teddiebox: rebooting into download mode");
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
    loop {
        // Raw counts as well as millivolts. The conversion rests on an
        // assumed attenuation and on GPIO9 measuring the pack rather than
        // something downstream of it, and a millivolt figure alone cannot
        // tell a wrong assumption from a flat battery.
        let pack_raw = adc.read_blocking(&mut battery);
        let charger_raw = adc.read_blocking(&mut charger);
        let pack_mv = power::battery_mv(pack_raw);
        let charger_mv = power::charger_mv(charger_raw);

        let state = match power::pack_state(pack_mv) {
            PackState::Healthy => "healthy",
            PackState::Low => "LOW",
            PackState::Critical => "CRITICAL",
        };
        esp_println::println!(
            "teddiebox: pack {pack_mv} mV ({state}, raw {pack_raw}), charger {charger_mv} mV (raw {charger_raw})"
        );

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
    match dac.reset().and_then(|()| dac.init()) {
        Ok(()) => esp_println::println!("teddiebox: codec configured"),
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
    if accel.init().is_err() {
        esp_println::println!("teddiebox: LIS3DH would not start");
        return;
    }

    loop {
        match accel.acceleration() {
            Ok([x, y, z]) => esp_println::println!("teddiebox: accel {x} {y} {z}"),
            Err(_) => esp_println::println!("teddiebox: LIS3DH read failed"),
        }
        Timer::after(Duration::from_secs(2)).await;
    }
}

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    let p = esp_hal::init(esp_hal::Config::default());
    let timg0 = TimerGroup::new(p.TIMG0);
    esp_rtos::start(timg0.timer0, p.FROM_CPU_INTR0);

    // Clear the ROM's force-download-boot request. It lives in the RTC domain
    // and survives a reset — that is what makes `dl` work — so leaving it set
    // would send every future reset back into download mode. Clearing it here
    // means the box can only ever be one reset away from running again.
    esp_hal::peripherals::LPWR::regs()
        .option1()
        .modify(|_, w| w.force_download_boot().clear_bit());

    let mut board = BoardPins::new(p.GPIO45, p.GPIO47, p.GPIO19, p.GPIO18, p.GPIO17);
    let mut gates = Gates::at_reset();

    spawner.spawn(heartbeat().unwrap());

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
    esp_println::println!("teddiebox: type dl<enter> to reboot into download mode");

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

    // A dim green breath, software PWM at 200 Hz. Bright enough to say the
    // firmware is alive, dim enough to live in a child's room.
    const PWM_PERIOD_US: u64 = 5_000;
    let lit = gates.led(Colour::Green).expect("the rail is up");
    let dark = gates.led(Colour::Off).expect("the rail is up");
    let mut since_poll = 0u32;

    loop {
        let duty = board::breathing_duty(Instant::now().as_millis() as u32);
        let on_us = PWM_PERIOD_US * duty as u64 / 255;

        if on_us > 0 {
            board.apply_all(&lit);
            Timer::after(Duration::from_micros(on_us)).await;
        }
        board.apply_all(&dark);
        Timer::after(Duration::from_micros(PWM_PERIOD_US - on_us)).await;

        // Roughly twice a second is responsive enough for a typed command and
        // cheap enough not to disturb the breath.
        since_poll += 1;
        if since_poll >= 100 {
            since_poll = 0;
            let mut buf = [0u8; 16];
            if let Ok(n) = console.read_buffered(&mut buf) {
                if buf[..n].iter().any(|&b| watch.feed(b)) {
                    reboot_to_download(&mut board, &mut gates);
                }
            }
        }
    }
}
