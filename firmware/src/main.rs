#![no_std]
#![no_main]

mod pins;

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_hal::timer::timg::TimerGroup;
use teddiebox_core::board::{Colour, Gates, Rail};

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

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    let p = esp_hal::init(esp_hal::Config::default());
    let timg0 = TimerGroup::new(p.TIMG0);
    esp_rtos::start(timg0.timer0, p.FROM_CPU_INTR0);

    let mut board = BoardPins::new(p.GPIO45, p.GPIO47, p.GPIO19, p.GPIO18, p.GPIO17);
    let mut gates = Gates::at_reset();

    spawner.spawn(heartbeat().unwrap());

    // The LED is on this rail, so it has to come up first.
    board.apply(gates.power(Rail::Peripherals, true));

    loop {
        for colour in [Colour::Red, Colour::Green, Colour::Blue] {
            match gates.led(colour) {
                Ok(levels) => board.apply_all(&levels),
                Err(_) => esp_println::println!("teddiebox: LED rail is down"),
            }
            Timer::after(Duration::from_millis(500)).await;
        }
    }
}
