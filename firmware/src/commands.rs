//! Executes a console command the run loop has decoded.
//!
//! Split out of `main`'s run loop: every arm here only stores into a request
//! atomic another task owns, or drives the board's own rails and gates —
//! dispatch, not another job for `main` to carry.

use core::sync::atomic::Ordering;

use esp_hal::peripherals::LPWR;
use teddiebox_console::Command;
use teddiebox_core::Event;

use crate::pins::BoardPins;
use crate::{
    audio, battery, go_dark, i2c_bus, inputs, led, nfc, not_in_this_build, ota, reboot,
    reboot_to_download, set_ears_skip, set_password, set_ssid, sleep_now, stack, FetchRequest,
    ASKED, AUTO_SLEEP, CONTENT_DIRECTORY, CONTENT_FILE, FETCH_REQUEST, LANGUAGE, NET_DOWN, NET_GET,
    NET_REQUEST, NET_SCAN, NET_STATUS, NET_TLS, NET_UP, PCM_FRAMES, REQUEST, REQUEST_CACHE,
    REQUEST_CONTENT, REQUEST_CRC, REQUEST_PCM, REQUEST_TAF, REQUEST_TONE, REQUEST_WALK,
    REQUEST_WAV, STAY_AWAKE,
};
use teddiebox_board::{Gates, Rail};

/// The command set a release image accepts: everything else asks for `dl`.
pub(crate) async fn release(
    command: Option<Command>,
    board: &mut BoardPins<'_>,
    gates: &mut Gates,
) {
    match command {
        Some(Command::DownloadMode) => {
            i2c_bus::quieten_codec().await;
            reboot_to_download(board, gates)
        }
        Some(_) => not_in_this_build(),
        None => {}
    }
}

/// The full console command set, available in a bench image.
pub(crate) async fn bench(
    command: Option<Command>,
    board: &mut BoardPins<'_>,
    gates: &mut Gates,
    lpwr: &mut Option<LPWR<'static>>,
    rgb: Option<&led::Rgb<'_>>,
) {
    match command {
        Some(Command::DownloadMode) => {
            i2c_bus::quieten_codec().await;
            reboot_to_download(board, gates)
        }
        Some(Command::Reboot) => {
            i2c_bus::quieten_codec().await;
            reboot(board, gates)
        }
        Some(Command::EnterSetup) => {
            esp_println::println!("teddiebox: setup asked for — restarting into setup mode");
            crate::setup::request();
            i2c_bus::quieten_codec().await;
            reboot(board, gates)
        }
        Some(Command::Tone) => {
            // The output is off until something plays, so every audio
            // command must switch it on first.
            i2c_bus::request_output(i2c_bus::OUTPUT_UP);
            REQUEST.store(REQUEST_TONE, Ordering::Relaxed);
        }
        Some(Command::PlayWav) => {
            board.apply(gates.power(Rail::Storage, true));
            i2c_bus::request_output(i2c_bus::OUTPUT_UP);
            REQUEST.store(REQUEST_WAV, Ordering::Relaxed);
        }
        Some(Command::Nfc) => {
            board.apply(gates.power(Rail::Storage, true));
            nfc::NFC_REQUEST.store(nfc::NFC_INVENTORY, Ordering::Relaxed);
        }
        Some(Command::Unlock) => {
            board.apply(gates.power(Rail::Storage, true));
            nfc::NFC_REQUEST.store(nfc::NFC_UNLOCK, Ordering::Relaxed);
        }
        Some(Command::ForceUnlock) => {
            board.apply(gates.power(Rail::Storage, true));
            nfc::NFC_REQUEST.store(nfc::NFC_FORCE_UNLOCK, Ordering::Relaxed);
        }
        Some(Command::CodecSet {
            page,
            register,
            value,
        }) => {
            if i2c_bus::codec_override_set(page, register, value) {
                esp_println::println!(
                    "teddiebox: codec page {page} register {register:#04x} -> {value:#04x} on next cinit"
                );
            } else {
                esp_println::println!("teddiebox: no override slots left — cclr first");
            }
        }
        Some(Command::CodecClear) => {
            i2c_bus::codec_overrides_clear();
            esp_println::println!("teddiebox: codec overrides cleared");
        }
        Some(Command::CodecInit) => {
            i2c_bus::CODEC_REINIT.store(true, Ordering::Relaxed);
        }
        Some(Command::CodecDown) => {
            i2c_bus::CODEC_POWER_DOWN.store(true, Ordering::Relaxed);
        }
        Some(Command::Output(on)) => {
            i2c_bus::request_output(if on {
                i2c_bus::OUTPUT_UP
            } else {
                i2c_bus::OUTPUT_DOWN
            });
        }
        Some(Command::Speaker(on)) => {
            i2c_bus::SPEAKER_REQUEST.store(
                if on {
                    i2c_bus::SPEAKER_UNMUTE
                } else {
                    i2c_bus::SPEAKER_MUTE
                },
                Ordering::Relaxed,
            );
        }
        Some(Command::HeadphoneStatus) => {
            i2c_bus::HEADPHONE_REPORT.store(true, Ordering::Relaxed);
        }
        // Sets both the static the codec bring-up reads and the event
        // for the reducer, which switches the output and its volume
        // scale together.
        Some(Command::Headphones(on)) => force_headphones(on),
        Some(Command::NetScan) => {
            NET_REQUEST.store(NET_SCAN, Ordering::Relaxed);
        }
        Some(Command::NetSsid(value)) => {
            esp_println::println!("teddiebox: net ssid set to {value}");
            set_ssid(value);
        }
        // Not echoed, like `pw`.
        Some(Command::NetPassword(value)) => {
            esp_println::println!("teddiebox: net passphrase set ({} chars)", value.len());
            set_password(value);
        }
        Some(Command::NetUp) => {
            NET_REQUEST.store(NET_UP, Ordering::Relaxed);
        }
        Some(Command::NetDown) => {
            NET_REQUEST.store(NET_DOWN, Ordering::Relaxed);
        }
        Some(Command::Get(ruid)) => queue_fetch(ruid),
        Some(Command::StackReport) => stack::report(),
        Some(Command::OtaStatus) => ota::status(),
        Some(Command::OtaWriteProbe) => ota::write_probe(),
        Some(Command::OtaBoot { slot }) => ota::arm_boot(slot),
        Some(Command::ReadToken) => {
            board.apply(gates.power(Rail::Storage, true));
            nfc::NFC_REQUEST.store(nfc::NFC_READ_TOKEN, Ordering::Relaxed);
        }
        Some(Command::PlayCache { directory, file }) => {
            board.apply(gates.power(Rail::Storage, true));
            CONTENT_DIRECTORY.store(directory, Ordering::Relaxed);
            CONTENT_FILE.store(file, Ordering::Relaxed);
            REQUEST.store(REQUEST_CACHE, Ordering::Relaxed);
        }
        Some(Command::Crc { directory, file }) => {
            board.apply(gates.power(Rail::Storage, true));
            CONTENT_DIRECTORY.store(directory, Ordering::Relaxed);
            CONTENT_FILE.store(file, Ordering::Relaxed);
            REQUEST.store(REQUEST_CRC, Ordering::Relaxed);
        }
        Some(Command::NetTls) => {
            NET_REQUEST.store(NET_TLS, Ordering::Relaxed);
        }
        Some(Command::NetStatus) => {
            NET_REQUEST.store(NET_STATUS, Ordering::Relaxed);
        }
        Some(Command::ReadMemory { first, count }) => {
            board.apply(gates.power(Rail::Storage, true));
            nfc::NFC_MEM_RANGE.store(
                (u32::from(first) << 8) | u32::from(count),
                Ordering::Relaxed,
            );
            nfc::NFC_REQUEST.store(nfc::NFC_READ_MEMORY, Ordering::Relaxed);
        }
        Some(Command::Lock) => {
            board.apply(gates.power(Rail::Storage, true));
            nfc::NFC_REQUEST.store(nfc::NFC_LOCK, Ordering::Relaxed);
        }
        // Sleep now, whatever `autosleep` says.
        Some(Command::Sleep) => {
            go_dark(board, gates, rgb).await;
            if let Err(reason) = sleep_now(lpwr).await {
                esp_println::println!(
                    "teddiebox: sleep not armed — {reason}; the box is awake and dark, \
                     and the ears are gone until rb"
                );
            }
        }
        Some(Command::StayAwake(on)) => {
            STAY_AWAKE.store(on, Ordering::Relaxed);
            esp_println::println!("teddiebox: staying awake {}", if on { "on" } else { "off" });
        }
        Some(Command::Revalidate) => {
            critical_section::with(|cs| ASKED.borrow_ref_mut(cs).forget_all());
            esp_println::println!(
                "teddiebox: every figure will be asked about again on its next placement"
            );
        }
        Some(Command::AutoSleep(on)) => {
            AUTO_SLEEP.store(on, Ordering::Relaxed);
            esp_println::println!(
                "teddiebox: the session ends in {}",
                if on { "deep sleep" } else { "a park" }
            );
        }
        Some(Command::EarsSkip(on)) => {
            set_ears_skip(on);
            esp_println::println!(
                "teddiebox: a held ear {}",
                if on {
                    "skips a chapter"
                } else {
                    "only changes the volume"
                }
            );
        }
        Some(Command::PcmCrc(on)) => {
            audio::PCM_CRC.store(on, Ordering::Relaxed);
            esp_println::println!(
                "teddiebox: playback checksum {} from the next story",
                if on { "on" } else { "off" }
            );
        }

        Some(Command::Plate(on)) => set_plate_polling(on, board, gates),
        Some(Command::Password(value)) => {
            nfc::NFC_PASSWORD.store(value, Ordering::Relaxed);
            // Not echoed: it is a credential, and console captures
            // are often saved to files.
            esp_println::println!("teddiebox: nfc password set");
        }
        Some(Command::PlayTaf) => {
            board.apply(gates.power(Rail::Storage, true));
            i2c_bus::request_output(i2c_bus::OUTPUT_UP);
            REQUEST.store(REQUEST_TAF, Ordering::Relaxed);
        }
        Some(Command::PlaySound { file }) => {
            board.apply(gates.power(Rail::Storage, true));
            i2c_bus::request_output(i2c_bus::OUTPUT_UP);
            CONTENT_DIRECTORY.store(LANGUAGE.content_directory(), Ordering::Relaxed);
            CONTENT_FILE.store(file, Ordering::Relaxed);
            REQUEST.store(REQUEST_CONTENT, Ordering::Relaxed);
        }
        Some(Command::PlayContent { directory, file }) => {
            board.apply(gates.power(Rail::Storage, true));
            i2c_bus::request_output(i2c_bus::OUTPUT_UP);
            CONTENT_DIRECTORY.store(directory, Ordering::Relaxed);
            CONTENT_FILE.store(file, Ordering::Relaxed);
            REQUEST.store(REQUEST_CONTENT, Ordering::Relaxed);
        }
        Some(Command::DumpPcm { frames }) => {
            board.apply(gates.power(Rail::Storage, true));
            PCM_FRAMES.store(frames, Ordering::Relaxed);
            REQUEST.store(REQUEST_PCM, Ordering::Relaxed);
        }
        Some(Command::BatteryLog { seconds }) => set_batlog(seconds),
        Some(Command::SlapTimeLimit { limit }) => {
            i2c_bus::SLAP_TIME_LIMIT.store(limit, Ordering::Relaxed);
        }
        Some(Command::SlapThreshold { threshold }) => {
            i2c_bus::SLAP_THRESHOLD.store(threshold, Ordering::Relaxed);
        }
        Some(Command::Stop) => {
            audio::STOP.store(true, Ordering::Relaxed);
        }
        // Only setup mode's console handles this; outside setup mode
        // the access point it configures is not running.
        Some(Command::SetupPassword(_)) => esp_println::println!(
            "teddiebox: setup pw only works in setup mode — type setup, or hold both ears at switch-on"
        ),
        Some(Command::Storage) => {
            // Switched on here, because this loop owns the pins, and
            // left on afterwards so the card never loses power mid-read.
            board.apply(gates.power(Rail::Storage, true));
            REQUEST.store(REQUEST_WALK, Ordering::Relaxed);
        }
        None => {}
    }
}

/// Sets both the static the codec bring-up reads and the event for the
/// reducer, which switches the output and its volume scale together.
fn force_headphones(on: bool) {
    i2c_bus::HEADPHONES_IN.store(on, Ordering::Relaxed);
    esp_println::println!(
        "teddiebox: headphones forced {}",
        if on { "in" } else { "out" }
    );
    if inputs::INPUT_EVENTS
        .try_send(Event::Headphones(on))
        .is_err()
    {
        esp_println::println!("teddiebox: input queue full, jack change dropped");
    }
}

/// Queues a `get`, attaching the token last read by the console `token`
/// command.
fn queue_fetch(ruid: [u8; 8]) {
    // One critical section: the token and the request are set together.
    critical_section::with(|cs| {
        let token = *nfc::TAG_TOKEN.borrow_ref(cs);
        *FETCH_REQUEST.borrow_ref_mut(cs) = Some(FetchRequest {
            ruid: u64::from_be_bytes(ruid),
            token,
            probe: false,
        });
    });
    NET_REQUEST.store(NET_GET, Ordering::Relaxed);
}

fn set_plate_polling(on: bool, board: &mut BoardPins<'_>, gates: &mut Gates) {
    if !on {
        nfc::PLATE_REPORT.store(true, Ordering::Relaxed);
    }
    if on {
        board.apply(gates.power(Rail::Storage, true));
    }
    nfc::PLATE_POLLING.store(on, Ordering::Relaxed);
    esp_println::println!("teddiebox: plate polling {}", if on { "on" } else { "off" });
}

fn set_batlog(seconds: u8) {
    battery::BATLOG_EVERY.store(seconds, Ordering::Relaxed);
    if seconds == 0 {
        esp_println::println!("teddiebox: batlog off");
    } else {
        esp_println::println!("teddiebox: batlog every {seconds} s");
        esp_println::println!("batlog,ms,raw,mv,playing,charger_raw");
    }
}
