//! The wake line and the two ears: the tasks that watch them, and the
//! channel their events reach the reducer through.

use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, Ordering};

use critical_section::Mutex as CsMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{with_timeout, Duration, Instant, Timer};
use esp_hal::gpio::Input;
use teddiebox_core::input::{self, Debounced, Edge};
use teddiebox_core::{Ear, Event};

use crate::{ears_skip, park_task, PARKED};

/// Input events (ear presses and slaps) waiting for the reducer.
///
/// A channel rather than a `Signal` like [`crate::nfc::PLATE_TAG`], because these
/// are events, not a state, and none may be lost: `Core` pairs each
/// `EarDown` with its `EarUp`. A signal keeps only the last value.
///
/// Eight is far more than the inputs produce between two reads (the media
/// task reads this every pass and every decoded frame), so a full queue means
/// something is wrong, and it is reported.
pub(crate) static INPUT_EVENTS: Channel<CriticalSectionRawMutex, Event, 8> = Channel::new();

#[embassy_executor::task]
/// Polls the wake line (button and charger) and prints its changes.
///
/// Polled, unlike the ears (see [`ear`]), because only this log line depends
/// on it. Also counts the raw changes while each change settles, to measure
/// switch bounce.
pub(crate) async fn inputs(wake: Input<'static>) {
    const POLL_MS: u64 = 2;

    let mut button = Debounced::released();
    // Raw changes since the last settled edge, counted on the raw line
    // (counting polls that differ from the settled state would always give
    // DEBOUNCE_MS / POLL_MS).
    let mut transitions = 0u32;
    let mut last_raw = false;

    loop {
        // Nothing to do for the rest of this power-on: see `PARKED`.
        if PARKED.load(Ordering::Relaxed) {
            park_task().await;
        }

        // The sleep code needs the wake line. Hand it over, then park rather
        // than return: a returning task drops its pin, which would leave the
        // pad in the wrong state for sleep.
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
            // One transition is the change itself; more is bounce. Bounce
            // faster than POLL_MS is not seen.
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

/// One ear, watched with GPIO interrupts rather than polling.
///
/// Polling every 2 ms lost about one tap in three while a story played:
/// decoding and card reads can keep this task from running for tens of
/// milliseconds, and a short press could start and end in between.
///
/// **The waits are on levels, not edges.** `wait_for` clears any pending
/// interrupt when it arms (see `unlisten_and_clear` in esp-hal's
/// `gpio/asynch.rs`), so an *edge* that happened while this task was not
/// running would be lost. A *level* interrupt fires at once if the pin is
/// already at that level.
#[embassy_executor::task(pool_size = 2)]
pub(crate) async fn ear(which: Ear, name: &'static str, mut pin: Input<'static>) {
    loop {
        // Park rather than return: returning drops the pin, which would leave
        // the pad in the wrong state for sleep.
        if PARKED.load(Ordering::Relaxed) || SLEEP_WANTED.load(Ordering::Relaxed) {
            park_task().await;
        }

        pin.wait_for_low().await;
        let down_at = Instant::now().as_millis();
        esp_println::println!("teddiebox: {name} pressed");
        send_input(Event::EarDown(which, down_at));

        // Bounce needs no extra handling: a release only counts once the
        // line has stayed high for `DEBOUNCE_MS`.
        //
        // A hold is also detected here: the reducer only sees events and
        // ticks once a second, so it cannot notice the moment a press becomes
        // a hold. This lets the chapter change while the ear is still held.
        let mut bounces = 0u32;
        let mut held = false;
        loop {
            if !held {
                // Measured from the press, not from this loop iteration.
                let so_far = Instant::now().as_millis().saturating_sub(down_at);
                let remaining = u64::from(input::LONG_PRESS_MS).saturating_sub(so_far);
                if with_timeout(Duration::from_millis(remaining), pin.wait_for_high())
                    .await
                    .is_err()
                {
                    held = true;
                    // With `ears_skip = no` the ears only change the volume.
                    // The hold is still printed but not sent to the reducer,
                    // and the release changes the volume like any press.
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

        // Timestamped when the release happened, not when it was confirmed
        // `DEBOUNCE_MS` later.
        let up_at = Instant::now()
            .as_millis()
            .saturating_sub(u64::from(input::DEBOUNCE_MS));
        esp_println::println!("teddiebox: {name} released, {bounces} bounces");
        send_input(Event::EarUp(which, up_at));
    }
}

/// Hands one input event to the reducer, or says why it could not.
///
/// A full queue means something is wrong (see [`INPUT_EVENTS`]), so it is
/// reported rather than dropped silently: a lost release would leave an ear
/// held forever.
fn send_input(event: Event) {
    if INPUT_EVENTS.try_send(event).is_err() {
        esp_println::println!("teddiebox: ear queue full — {event:?} dropped");
    }
}

/// Asks the task that polls the wake line to hand the pin over.
///
/// The console loop enters sleep (it owns the rails, the codec and the `LPWR`
/// peripheral), but the pin belongs to `inputs`, which polls it. Arming the
/// wake and entering sleep must happen together in one task: esp-hal clears
/// a level interrupt when it fires, and that is the same bit sleep entry
/// reads, so an ear pressed in between would cause a panic.
pub(crate) static SLEEP_WANTED: AtomicBool = AtomicBool::new(false);

/// Where the wake line waits between the two tasks.
///
/// `inputs` stops polling as soon as it puts the pin here.
/// [`crate::sleep_now`] puts it back if sleep fails, so `sleep` can be tried
/// again.
pub(crate) static WAKE_LINE: CsMutex<RefCell<Option<Input<'static>>>> =
    CsMutex::new(RefCell::new(None));
