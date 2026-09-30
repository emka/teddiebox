//! The setup portal's boot path.
//!
//! Split out of `main`: entered once, before any of the normal boot's tasks
//! exist, and never returns — the access point serves `CONFIG.TXT` for as
//! long as the box stays in this mode.

use core::sync::atomic::Ordering;

use portable_atomic::AtomicU32;
use teddiebox_core::setup_request;

use embassy_time::{Duration, Timer};
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::peripherals::{GPIO34, GPIO35, GPIO36, GPIO38, GPIO44, SPI2, UART0, WIFI};
use esp_hal::spi::master::Spi;
use teddiebox_board::{self as board, Gates, Rail};
use teddiebox_core::{colour_for, LedState};

use crate::pins::BoardPins;
use crate::{audio, battery, led, net, portal, storage};

/// A request for setup mode that survives the software reset between the
/// `setup` command and the next boot. See [`setup_request`] for why only one
/// exact word counts.
#[esp_hal::ram(unstable(rtc_fast, persistent))]
static REQUEST: AtomicU32 = AtomicU32::new(0);

/// Asks the next boot for setup mode. The caller then resets.
pub(crate) fn request() {
    REQUEST.store(setup_request::REQUESTED, Ordering::Relaxed);
}

/// Whether the run before this boot asked for setup mode.
///
/// Clears the request, so only this boot answers it and Restart in the
/// portal boots normally.
pub(crate) fn take_request() -> bool {
    let word = REQUEST.load(Ordering::Relaxed);
    REQUEST.store(0, Ordering::Relaxed);
    setup_request::is_requested(word)
}

/// Starts the setup portal instead of the normal boot, when both ears are
/// held at power-on or the console's `setup` command asked for it. Checked
/// before `net` is started (the only other user of `p.WIFI`) and before the
/// rest of the boot, because setup mode runs almost none of the tasks below:
/// no decoder, codec, NFC or media loop.
///
/// Skipping those tasks does not make room for the portal: its buffers are
/// held across `await` points, so they are part of this task's future in
/// `.bss`, and the stack gets whatever `.bss` leaves. See the measurements
/// in `portal.rs`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn enter(
    board: &mut BoardPins<'_>,
    gates: &mut Gates,
    rgb: Option<&led::Rgb<'_>>,
    spi2: SPI2<'static>,
    sck: GPIO35<'static>,
    mosi: GPIO38<'static>,
    miso: GPIO36<'static>,
    cs: GPIO34<'static>,
    wifi: WIFI<'static>,
    uart0: UART0<'static>,
    uart_rx: GPIO44<'static>,
) -> ! {
    // Nothing drains `INPUT_EVENTS` here: the reducer lives in the media
    // task and that is never spawned. Told now rather than discovered
    // later, so `sense` keeps its readings to the console instead of
    // filling an eight-slot queue and then complaining about it once
    // every two seconds for as long as the portal is up.
    battery::SETUP_MODE.store(true, Ordering::Relaxed);

    // Nothing before this point brings the card up: the rest of the boot
    // does that lazily, inside `media`, on the first command that needs
    // it. Setup mode never reaches `media`, so the same bus and the same
    // rail are brought up here instead, once, so `portal::run` has the
    // card it needs to read and rewrite `CONFIG.TXT`.
    board.apply(gates.power(Rail::Storage, true));
    Timer::after(Duration::from_millis(50)).await;

    // In other modes the main loop shows `LED_REQUEST`. This path never
    // reaches that loop, so it sets the LED directly.
    let paint = |state: LedState| {
        if let Some(rgb) = rgb {
            if let Ok(lit) = gates.led(colour_for(state)) {
                rgb.apply(&lit, board::LED_DUTY);
            }
        }
    };

    // Taken before the card is mounted, because the failure path below
    // parks, which is an `await`: anything alive across it becomes part
    // of this task's future, in `.bss`. Doing this first keeps the
    // 812-byte `Mounted` out of the future.
    let Some(scratch) = audio::take_scratch_bytes() else {
        esp_println::println!("teddiebox: portal cannot have the decode scratch — it is in use");
        paint(LedState::Error);
        portal::park().await
    };

    // The access point starts even without a card: the page then says
    // what is wrong, and only saving is refused. The console prints the
    // exact reason.
    let card = match Spi::new(spi2, storage::init_config()) {
        Ok(spi) => {
            let spi = spi.with_sck(sck).with_mosi(mosi).with_miso(miso);
            let cs = Output::new(cs, Level::High, OutputConfig::default());
            match storage::Mounted::open(spi, cs, esp_hal::delay::Delay::new()) {
                Ok(card) => Some(card),
                Err(reason) => {
                    esp_println::println!("teddiebox: portal could not mount the card — {reason}");
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

    // Setup mode reuses the decoder's scratch memory. `portal::run`'s
    // future (socket buffers, the card and the radio's `StackResources`)
    // is moved into `audio::SCRATCH`, which the
    // decoder does not use in this mode, so only a pointer stays in this
    // task's future. That keeps the stack as large as without the portal;
    // `portal::place` has the numbers.
    //
    // The scratch is taken through the same check the decoder uses, so
    // both can never have it at once.
    let Some(portal) = portal::place(
        scratch,
        portal::run(wifi, uart0, uart_rx, card, seed, paint),
    ) else {
        // `place` has already printed what did not fit. Park rather than
        // reset: the ears may still be held, so a reset would come
        // straight back here.
        portal::park().await
    };
    portal.await
}
