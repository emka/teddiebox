#![no_std]
#![no_main]

mod pins;

use embassy_executor::Spawner;
use embassy_time::{Duration, Instant, Timer};
use esp_backtrace as _;
use esp_hal::gpio::{Input, InputConfig, Pull};
use esp_hal::timer::timg::TimerGroup;
use esp_hal::uart::{Config as UartConfig, UartRx};
use teddiebox_core::board::{Colour, Gates, Rail};
use teddiebox_core::console::CommandWatch;
use teddiebox_core::input::{self, Debounced, Edge};

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

    let mut state = [
        ("left ear", Debounced::released(), 0u32),
        ("right ear", Debounced::released(), 0u32),
        ("wake", Debounced::released(), 0u32),
    ];

    loop {
        let now = Instant::now().as_millis() as u32;
        let raw = [
            input::ear_pressed(left.is_high()),
            input::ear_pressed(right.is_high()),
            input::wake_asserted(wake.is_high()),
        ];

        for ((name, button, flips), &pressed) in state.iter_mut().zip(raw.iter()) {
            let was = button.is_pressed();
            match button.update(pressed, now) {
                Some(edge) => {
                    let label = match edge {
                        Edge::Pressed => "pressed",
                        Edge::Released => "released",
                    };
                    esp_println::println!("teddiebox: {name} {label} after {flips} bounces");
                    *flips = 0;
                }
                None if pressed != was => *flips += 1,
                None => {}
            }
        }

        Timer::after(Duration::from_millis(POLL_MS)).await;
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

    // The LED is on this rail, so it has to come up first.
    board.apply(gates.power(Rail::Peripherals, true));

    // UART0's receive half. esp-println keeps the transmit half.
    let mut console = UartRx::new(p.UART0, UartConfig::default().with_baudrate(115200))
        .expect("UART0 receive")
        .with_rx(p.GPIO44);
    let mut watch = CommandWatch::new();
    esp_println::println!("teddiebox: type dl<enter> to reboot into download mode");

    loop {
        for colour in [Colour::Red, Colour::Green, Colour::Blue] {
            match gates.led(colour) {
                Ok(levels) => board.apply_all(&levels),
                Err(_) => esp_println::println!("teddiebox: LED rail is down"),
            }

            // Show the colour for half a second, staying responsive to the
            // console throughout rather than only between colours.
            for _ in 0..10 {
                Timer::after(Duration::from_millis(50)).await;
                let mut buf = [0u8; 16];
                if let Ok(n) = console.read_buffered(&mut buf) {
                    if buf[..n].iter().any(|&b| watch.feed(b)) {
                        reboot_to_download(&mut board, &mut gates);
                    }
                }
            }
        }
    }
}
