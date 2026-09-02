#![no_std]
#![no_main]

mod audio;
mod led;
mod libc_shim;
mod nfc;
mod pins;
mod storage;

use core::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use embassy_executor::Spawner;
use embassy_time::{Duration, Instant, Timer};

use esp_backtrace as _;
use esp_hal::analog::adc::{Adc, AdcConfig, Attenuation};
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::i2s::master::{Channels, DataFormat, I2s, TdmConfig};
use esp_hal::spi::master::Spi;
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_hal::uart::{Config as UartConfig, UartRx};
use lis3dh::{regs as lis, Lis3dh};
use teddiebox_core::board::{self, Colour, Gates, Rail};
use teddiebox_core::console::{Command, CommandWatch};
use teddiebox_core::i2c as bus;
use teddiebox_core::input::{self, Debounced, Edge};
use teddiebox_core::power::{self, PackState};
use teddiebox_core::tone;
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
/// Reboots straight back into the application.
///
/// The rails go down first, same as the download path. Exists so a laptop can
/// restart the box without `esptool`, which takes exclusive hold of the serial
/// port and so cannot run while anything is watching the console.
fn reboot(board: &mut BoardPins, gates: &mut Gates) -> ! {
    esp_println::println!("teddiebox: rebooting");
    board.apply_all(&gates.release_for_reset());
    esp_hal::system::software_reset()
}

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
    let mut dac_delay = esp_hal::delay::Delay::new();
    match dac.reset().and_then(|()| dac.init(&mut dac_delay)) {
        Ok(()) => {
            esp_println::println!("teddiebox: codec configured");

            // A bring-up listening level, not a design decision — real volume
            // belongs to `teddiebox_core::VolumeModel` and the ears, once
            // there is a box to drive them from.
            //
            // -12 dB was tuned against the test tone, which peaks at half
            // scale. Real content runs to full scale and carries far more
            // spectral energy than a sine, and the first Tonie played through
            // this was, in the listener's words, "120% of the max volume".
            // The codec goes to -63.5 dB, so erring quiet costs nothing and
            // is the right way to err beside someone's head.
            if dac.set_volume_db(BENCH_VOLUME_DB).is_err() {
                esp_println::println!("teddiebox: codec volume not set");
            }

            // What it says it did, rather than what we asked for. A silent
            // output with every configuration register correct is exactly the
            // case this separates: asked wrongly, or declined.
            // Read after `init` has waited out the drivers' ramp. Read before
            // it, HPL reports itself unpowered for 304 ms — measured — which
            // is what "step 6's headphone half is unproven" rested on.
            match dac.power_flags() {
                Ok(f) => esp_println::println!(
                    "teddiebox: codec powered dac_l={} dac_r={} class_d_l={} class_d_r={} hpl={}",
                    f.left_dac,
                    f.right_dac,
                    f.left_class_d,
                    f.right_class_d,
                    f.hpl_driver
                ),
                Err(_) => esp_println::println!("teddiebox: codec flags unreadable"),
            }
        }
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

/// How loud the box plays during bring-up.
///
/// Deliberately low. Turning it up is a one-line change and a reflash; the
/// alternative is discovering it is too loud with the box against your ear.
const BENCH_VOLUME_DB: i8 = -35;

/// What the console has asked the media task to do.
///
/// One word rather than a flag each, because the tone, the card walk and WAV
/// playback all contend for the same two pieces of hardware — the I2S
/// peripheral and the SD bus — and a task that owns both is the honest way to
/// say that. None of them runs at boot: the tone is loud, the walk powers a
/// rail shared with the NFC reader, and the box is usually sitting next to
/// whoever is working on it.
static REQUEST: AtomicU8 = AtomicU8::new(REQUEST_NONE);
const REQUEST_NONE: u8 = 0;
const REQUEST_TONE: u8 = 1;
const REQUEST_WALK: u8 = 2;
const REQUEST_WAV: u8 = 3;
const REQUEST_TAF: u8 = 4;

/// What the console has asked the NFC reader to do.
///
/// Separate from [`REQUEST`] because the reader has its own SPI bus and shares
/// only the power rail, so it has no reason to queue behind a track that is
/// still playing.
static NFC_REQUEST: AtomicU8 = AtomicU8::new(REQUEST_NONE);
const NFC_INVENTORY: u8 = 1;
const NFC_UNLOCK: u8 = 2;
const NFC_FORCE_UNLOCK: u8 = 3;
const NFC_LOCK: u8 = 4;

/// The SLIX privacy password, as typed at the console.
///
/// RAM only, and deliberately: it is a credential, it is never written to the
/// card or committed, and it dies with the next reset.
static NFC_PASSWORD: AtomicU32 = AtomicU32::new(0);

/// Owns the I2S peripheral and the SD bus, and serves the bench commands that
/// need them.
///
/// One task for three jobs because they contend for the same two pieces of
/// hardware. The card is mounted once and kept, so `sd` and `wav` do not each
/// re-identify it and re-raise the bus clock.
///
/// The tone and WAV playback are both terminal: each takes the I2S peripheral
/// and does not give it back — the tone by design, since it repeats forever,
/// and playback because its DMA buffer is a `static` claimed once whose
/// pre-fill state this does not re-derive. `rb` restarts the box, which is the
/// documented way to run another.
#[embassy_executor::task]
async fn media(
    spi: Spi<'static, esp_hal::Blocking>,
    cs: Output<'static>,
    i2s_tx: esp_hal::i2s::master::I2sTx<'static, esp_hal::Blocking>,
    mut tone_buffer: esp_hal::dma::DmaLoopBuf,
    wav_buffer: esp_hal::dma::DmaTxStreamBuf,
) {
    // Exactly one cycle of a sine, looped by the DMA forever.
    //
    // A stream buffer was tried first and underran: the transfer starts as
    // soon as it is created, ran off the end of an empty buffer before the
    // first sample was pushed, and never restarted. A fixed repeating waveform
    // wants a buffer the DMA repeats rather than one the CPU refills.
    for (i, sample) in tone::SINE.iter().enumerate() {
        let bytes = sample.to_le_bytes();
        // Interleaved stereo, the same sample in both slots. The codec takes
        // its speaker path from the left channel and its headphones from both,
        // so a single channel would make the result depend on which output is
        // being listened to.
        tone_buffer[i * 4] = bytes[0];
        tone_buffer[i * 4 + 1] = bytes[1];
        tone_buffer[i * 4 + 2] = bytes[0];
        tone_buffer[i * 4 + 3] = bytes[1];
    }

    // Every one of these is claimed once and not returned, so each is held in
    // an Option and taken when its command arrives.
    let mut card: Option<storage::Mounted> = None;
    let mut bus = Some((spi, cs));
    let mut i2s_tx = Some(i2s_tx);
    let mut tone_buffer = Some(tone_buffer);
    let mut wav_buffer = Some(wav_buffer);
    // The tone's transfer stops when it is dropped, so it is parked here for
    // as long as the tone should play — which is until the box restarts.
    let mut _tone_transfer = None;

    loop {
        let request = REQUEST.swap(REQUEST_NONE, Ordering::Relaxed);
        if request == REQUEST_NONE {
            Timer::after(Duration::from_millis(100)).await;
            continue;
        }

        // The console loop raised the storage rail before setting the request.
        // Devices need their supply settled before they answer — a scan against
        // an unsettled rail is what invented an I2C device at 0x09 in step 4.
        if matches!(request, REQUEST_WALK | REQUEST_WAV | REQUEST_TAF) && card.is_none() {
            let Some((spi, cs)) = bus.take() else {
                esp_println::println!("teddiebox: the card bus is gone — reboot to retry");
                continue;
            };
            Timer::after(Duration::from_millis(50)).await;
            match storage::Mounted::open(spi, cs, esp_hal::delay::Delay::new()) {
                Ok(mounted) => card = Some(mounted),
                Err(reason) => {
                    // The bus was consumed by the attempt, so nothing needing
                    // the card can be retried without a restart.
                    esp_println::println!("teddiebox: sd failed — {reason}");
                    continue;
                }
            }
        }

        match request {
            REQUEST_WALK => {
                if let Some(card) = card.as_ref() {
                    card.walk().await;
                }
            }

            REQUEST_TONE => {
                let (Some(tx), Some(buffer)) = (i2s_tx.take(), tone_buffer.take()) else {
                    esp_println::println!("teddiebox: I2S is already in use");
                    continue;
                };
                match tx.write(buffer) {
                    Ok(transfer) => {
                        esp_println::println!(
                            "teddiebox: playing {} Hz at {} Hz",
                            tone::TONE_HZ,
                            tone::SAMPLE_RATE_HZ
                        );
                        _tone_transfer = Some(transfer);
                    }
                    Err(_) => esp_println::println!("teddiebox: I2S would not start"),
                }
            }

            REQUEST_WAV | REQUEST_TAF => {
                let Some(card) = card.as_ref() else {
                    continue;
                };
                // Checked before anything is taken. Building a tuple of takes
                // and matching on it afterwards drops whichever resource did
                // come back when the other did not — which quietly destroyed
                // the DMA buffer a later command still needed.
                if i2s_tx.is_none() || wav_buffer.is_none() {
                    esp_println::println!(
                        "teddiebox: the audio hardware is already claimed — rb to run another"
                    );
                    continue;
                }
                let (Some(tx), Some(buffer)) = (i2s_tx.take(), wav_buffer.take()) else {
                    continue;
                };

                let outcome = if request == REQUEST_WAV {
                    audio::play_first_wav(card, tx, buffer).await
                } else {
                    audio::play_first_taf(card, tx, buffer).await
                };
                if let Err(reason) = outcome {
                    esp_println::println!("teddiebox: playback failed — {reason}");
                }
            }

            _ => {}
        }
    }
}

/// Brings the NFC reader up on first use and answers the bench commands.
///
/// Waits for a request rather than starting at boot: the reader shares the
/// storage rail, and raising that rail is the console loop's decision.
#[embassy_executor::task]
async fn nfc_reader(
    spi: Spi<'static, esp_hal::Blocking>,
    cs: Output<'static>,
    irq: Input<'static>,
) {
    while NFC_REQUEST.load(Ordering::Relaxed) == REQUEST_NONE {
        Timer::after(Duration::from_millis(100)).await;
    }

    // The rail was raised by the console loop; give it the same settling time
    // the card and the I2C devices get.
    Timer::after(Duration::from_millis(50)).await;

    let mut reader = match nfc::Reader::open(spi, cs, irq, esp_hal::delay::Delay::new()) {
        Ok(reader) => reader,
        Err(reason) => {
            esp_println::println!("teddiebox: nfc failed — {reason}");
            return;
        }
    };

    loop {
        match NFC_REQUEST.swap(REQUEST_NONE, Ordering::Relaxed) {
            NFC_INVENTORY => {
                reader.inventory();

                // Is the antenna even connected? With our own field off, the
                // RSSI register reports RF arriving from outside, so an
                // external source proves the coil is coupled to the chip.
                // Nothing else here can tell a disconnected antenna from an
                // empty plate.
                esp_println::println!(
                    "teddiebox: nfc listening for an external field for 6 s — \
                     hold an NFC phone against the plate"
                );
                reader.set_field(false);
                let mut peak = 0u8;
                for _ in 0..60 {
                    peak = peak.max(reader.rssi());
                    Timer::after(Duration::from_millis(100)).await;
                }
                reader.set_field(true);
                if peak == 0 {
                    esp_println::println!(
                        "teddiebox: nfc heard nothing at all — the antenna is not coupled"
                    );
                } else {
                    esp_println::println!(
                        "teddiebox: nfc external field peaked at {peak:#04x} — the antenna works"
                    );
                }
            }
            NFC_UNLOCK => {
                let password = NFC_PASSWORD.load(Ordering::Relaxed);
                if password == 0 {
                    esp_println::println!("teddiebox: nfc no password set — type `pw <8 hex>`");
                } else {
                    reader.unlock(password);
                }
            }
            NFC_FORCE_UNLOCK => {
                let password = NFC_PASSWORD.load(Ordering::Relaxed);
                if password == 0 {
                    esp_println::println!("teddiebox: nfc no password set — type `pw <8 hex>`");
                } else {
                    reader.force_unlock(password);
                }
            }
            NFC_LOCK => {
                let password = NFC_PASSWORD.load(Ordering::Relaxed);
                if password == 0 {
                    esp_println::println!("teddiebox: nfc no password set — type `pw <8 hex>`");
                } else {
                    reader.lock(password);
                }
            }
            _ => {}
        }
        Timer::after(Duration::from_millis(100)).await;
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

    let mut board = BoardPins::new(p.GPIO45, p.GPIO47);
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
    esp_println::println!(
        "teddiebox: dl rb | t wav taf (loud) | sd | nfc, pw <8 hex>, slix, slixp, lock"
    );

    // Audio out on I2S: DIN 10, BCLK 11, WCLK 12, at the rate the codec's PLL
    // was configured for. The SD card is SPI2 on CLK 35, MOSI 38, MISO 36 with
    // CS 34, created at the specification's 400 kHz initialisation rate;
    // storage.rs raises it once the card has identified itself.
    //
    // Both go to one task: the tone, the checksum walk and WAV playback all
    // contend for these two peripherals.
    match (
        I2s::new(
            p.I2S0,
            p.DMA_CH0,
            TdmConfig::new_tdm_philips()
                .with_sample_rate(Rate::from_hz(tone::SAMPLE_RATE_HZ))
                .with_data_format(DataFormat::Data16Channel16)
                .with_channels(Channels::STEREO),
        ),
        Spi::new(p.SPI2, storage::init_config()),
    ) {
        (Ok(i2s), Ok(spi)) => {
            let i2s_tx = i2s
                .i2s_tx
                .with_bclk(p.GPIO11)
                .with_ws(p.GPIO12)
                .with_dout(p.GPIO10)
                .build();
            let spi = spi
                .with_sck(p.GPIO35)
                .with_mosi(p.GPIO38)
                .with_miso(p.GPIO36);
            // Idle high: on SPI a card watches for its select line to fall, and
            // one that starts low is addressed before anything is ready to talk
            // to it.
            let cs = Output::new(p.GPIO34, Level::High, OutputConfig::default());

            let tone_buffer = esp_hal::dma_loop_buffer!(tone::SINE.len() * 4);
            // Roughly 170 ms of audio at 48 kHz stereo 16-bit. Design §5 wants
            // the cushion sized from measurement rather than estimate, and this
            // is the buffer whose low-water mark provides that measurement.
            let wav_buffer = esp_hal::dma_tx_stream_buffer!(audio::BUFFER_BYTES);

            spawner.spawn(media(spi, cs, i2s_tx, tone_buffer, wav_buffer).unwrap());

            // The reader is on its own bus: SCLK 4, MOSI 2, MISO 3, CS 1, with
            // IRQ on 13. It shares only the power rail with the card.
            match Spi::new(p.SPI3, nfc::bus_config()) {
                Ok(nfc_spi) => {
                    let nfc_spi = nfc_spi
                        .with_sck(p.GPIO4)
                        .with_mosi(p.GPIO2)
                        .with_miso(p.GPIO3);
                    let nfc_cs = Output::new(p.GPIO1, Level::High, OutputConfig::default());
                    let nfc_irq = Input::new(p.GPIO13, InputConfig::default());
                    spawner.spawn(nfc_reader(nfc_spi, nfc_cs, nfc_irq).unwrap());
                }
                Err(_) => esp_println::println!("teddiebox: NFC SPI would not configure"),
            }
        }
        (Err(_), _) => esp_println::println!("teddiebox: I2S would not configure"),
        (_, Err(_)) => esp_println::println!("teddiebox: SPI would not configure"),
    }

    // Devices need a moment after their rail comes up before they answer.
    // Without this the accelerometer misses the scan and then answers the
    // probe a few milliseconds later, which reads as a bus that is lying.
    Timer::after(Duration::from_millis(50)).await;

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

    // A dim green breath. The waveform is held by the LEDC peripheral, so
    // this loop only decides how bright and then goes back to sleep — it does
    // not have to be on time. The software PWM this replaces woke four hundred
    // times a second and could not keep its own period once anything else
    // wanted the executor.
    const BREATH_STEP: Duration = Duration::from_millis(20);

    // The controller and its timer are bound here rather than inside the
    // helper because the channels borrow the timer, so it has to outlive them.
    // `main` never returns, which is exactly long enough.
    let ledc = led::controller(p.LEDC);
    let timer = led::timer(&ledc);
    let rgb = match timer.as_ref() {
        Ok(timer) => match led::Rgb::new(&ledc, timer, p.GPIO19, p.GPIO18, p.GPIO17) {
            Ok(rgb) => Some(rgb),
            Err(()) => {
                esp_println::println!("teddiebox: LED channels would not configure");
                None
            }
        },
        Err(()) => {
            esp_println::println!("teddiebox: LED timer would not configure");
            None
        }
    };

    let lit = gates.led(Colour::Green).expect("the rail is up");

    loop {
        if let Some(rgb) = rgb.as_ref() {
            rgb.apply(
                &lit,
                board::breathing_duty(Instant::now().as_millis() as u32),
            );
        }

        let mut buf = [0u8; 16];
        if let Ok(n) = console.read_buffered(&mut buf) {
            match buf[..n].iter().find_map(|&b| watch.feed(b)) {
                Some(Command::DownloadMode) => reboot_to_download(&mut board, &mut gates),
                Some(Command::Reboot) => reboot(&mut board, &mut gates),
                Some(Command::Tone) => {
                    REQUEST.store(REQUEST_TONE, Ordering::Relaxed);
                }
                Some(Command::PlayWav) => {
                    board.apply(gates.power(Rail::Storage, true));
                    REQUEST.store(REQUEST_WAV, Ordering::Relaxed);
                }
                Some(Command::Nfc) => {
                    board.apply(gates.power(Rail::Storage, true));
                    NFC_REQUEST.store(NFC_INVENTORY, Ordering::Relaxed);
                }
                Some(Command::Unlock) => {
                    board.apply(gates.power(Rail::Storage, true));
                    NFC_REQUEST.store(NFC_UNLOCK, Ordering::Relaxed);
                }
                Some(Command::ForceUnlock) => {
                    board.apply(gates.power(Rail::Storage, true));
                    NFC_REQUEST.store(NFC_FORCE_UNLOCK, Ordering::Relaxed);
                }
                Some(Command::Lock) => {
                    board.apply(gates.power(Rail::Storage, true));
                    NFC_REQUEST.store(NFC_LOCK, Ordering::Relaxed);
                }
                Some(Command::Password(value)) => {
                    NFC_PASSWORD.store(value, Ordering::Relaxed);
                    // Deliberately not echoed. It is a credential, and a bench
                    // capture is a file that outlives the session.
                    esp_println::println!("teddiebox: nfc password set");
                }
                Some(Command::PlayTaf) => {
                    board.apply(gates.power(Rail::Storage, true));
                    REQUEST.store(REQUEST_TAF, Ordering::Relaxed);
                }
                Some(Command::Storage) => {
                    // The rail comes up here because this loop owns the pins.
                    // It stays up afterwards: the walk is a bench action, and a
                    // rail that drops under a card mid-read is a worse bug than
                    // one left on.
                    board.apply(gates.power(Rail::Storage, true));
                    REQUEST.store(REQUEST_WALK, Ordering::Relaxed);
                }
                None => {}
            }
        }

        Timer::after(BREATH_STEP).await;
    }
}
