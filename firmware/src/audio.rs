//! Playing a WAV off the card, straight at the codec.
//!
//! Bench step 8: several minutes of audio read from SD and pushed through I2S
//! without a gap, with the buffer occupancy logged. It joins steps 6 and 7,
//! and it is the first time anything on this box has had a deadline.
//!
//! WAV rather than TAF on purpose. With a decoder in the middle, an underrun
//! and a decode fault sound the same, and the question here is only whether
//! bytes can be got off the card fast enough and handed over in time.
//!
//! The accounting is [`teddiebox_core::cushion`] and the header parsing is
//! [`teddiebox_core::wav`]; both are tested on the host. What is left here is
//! the part that owns a DMA engine.

use embassy_futures::yield_now;
use embassy_time::{Duration, Instant};
use embedded_sdmmc::RawFile;
use esp_hal::dma::DmaTxStreamBuf;
use esp_hal::i2s::master::I2sTx;
use teddiebox_core::cushion::Cushion;
use teddiebox_core::wav::{WavError, WavFormat};

use crate::storage::Mounted;

/// The DMA buffer between the card and the codec.
///
/// Roughly 170 ms at 48 kHz stereo 16-bit. Design §5 estimates a 500 ms
/// cushion at about 96 KB and says explicitly to size it from measurement, so
/// this starts smaller and the low-water mark it reports is what settles the
/// real figure.
pub const BUFFER_BYTES: usize = 32_768;

/// Bytes read from the card in one go.
///
/// A whole number of 512-byte blocks, and large enough that the fixed cost of
/// a read is spread over useful work.
const CHUNK: usize = 4096;

/// How often the occupancy line is printed.
const LOG_EVERY: Duration = Duration::from_secs(5);

type Blocking = esp_hal::Blocking;

/// Plays the first `.WAV` in the card's root directory.
///
/// Terminal, like the test tone: the DMA buffer is a `static` claimed once,
/// and its pre-fill state is not something this re-derives between plays. `rb`
/// restarts the box, which is the documented way to play it again.
pub async fn play_first_wav(
    card: &Mounted,
    i2s_tx: I2sTx<'static, Blocking>,
    mut buffer: DmaTxStreamBuf,
) -> Result<(), &'static str> {
    let Some((name, size)) = card.find_wav() else {
        return Err("no .WAV in the card's root directory");
    };
    esp_println::println!("teddiebox: wav {name}, {size} bytes");

    let file = card.open_file(name)?;

    // Enough for the header even with a chunk or two in front of `data`.
    let mut head = [0u8; 256];
    let read = card.read(file, &mut head)?;
    let format = WavFormat::parse(&head[..read]).map_err(describe)?;

    esp_println::println!(
        "teddiebox: wav {} Hz, {} ch, {} bit, {} ms",
        format.sample_rate,
        format.channels,
        format.bits_per_sample,
        format.duration_ms()
    );

    // Step 6 configured I2S and the codec once, at boot. A file in another
    // format would play at the wrong speed rather than fail, which is a
    // miserable thing to diagnose by ear.
    if !format.matches_codec() {
        card.close_file(file);
        return Err("the file is not 48 kHz stereo 16-bit");
    }

    card.seek(file, format.data_offset)?;

    // Pre-fill before the transfer starts. `write()` begins the transfer as it
    // is created, so a buffer that is still empty at that moment is played as
    // silence and the stream runs off its end before the first sample arrives
    // — which is exactly how the step 6 tone failed.
    let capacity = prefill(card, file, &mut buffer)?;
    // Prefill reads ahead of what it could fit, so put the file back exactly
    // where the buffer ends.
    card.seek(file, format.data_offset + capacity as u32)?;
    esp_println::println!("teddiebox: wav buffer {capacity} bytes pre-filled");

    let mut remaining = format.data_len as usize - capacity;
    let mut transfer = match i2s_tx.write(buffer) {
        Ok(transfer) => transfer,
        Err(_) => {
            card.close_file(file);
            return Err("I2S would not start");
        }
    };

    let mut cushion = Cushion::new(capacity as u32);
    cushion.start();
    let mut chunk = [0u8; CHUNK];
    let mut last_log = Instant::now();
    let started = Instant::now();

    while remaining > 0 {
        let free = transfer.available_bytes();
        cushion.observe(capacity.saturating_sub(free) as u32);

        if free >= CHUNK.min(remaining) {
            let want = CHUNK.min(remaining);
            let read = card.read(file, &mut chunk[..want])?;
            if read == 0 {
                // The header's length disagreed with the file. Not fatal, but
                // it means the file is shorter than it claims.
                esp_println::println!("teddiebox: wav ended {remaining} bytes early");
                break;
            }

            // `push` takes what fits and no more, so the remainder has to be
            // offered again rather than dropped.
            let mut offset = 0;
            while offset < read {
                let pushed = transfer.push(&chunk[offset..read]);
                offset += pushed;
                if pushed == 0 {
                    yield_now().await;
                }
            }
            remaining -= read;
        }

        if last_log.elapsed() >= LOG_EVERY {
            last_log = Instant::now();
            esp_println::println!(
                "teddiebox: wav {} s, buffer {}% (low {}%), {} underruns",
                started.elapsed().as_secs(),
                cushion.percent(),
                cushion.low_water_percent(),
                cushion.underruns()
            );
        }

        yield_now().await;
    }

    // Let the DMA drain what is still buffered, without spinning on it.
    while !transfer.is_done() {
        yield_now().await;
    }

    card.close_file(file);
    esp_println::println!(
        "teddiebox: wav done — {} s, low water {}% ({} bytes), {} underruns",
        started.elapsed().as_secs(),
        cushion.low_water_percent(),
        cushion.low_water(),
        cushion.underruns()
    );
    Ok(())
}

/// Fills the DMA buffer from the card before the transfer starts.
///
/// Returns how many bytes went in, which is the buffer's usable capacity and
/// therefore the denominator for every occupancy figure afterwards.
fn prefill(
    card: &Mounted,
    file: RawFile,
    buffer: &mut DmaTxStreamBuf,
) -> Result<usize, &'static str> {
    let mut filled = 0;
    let mut chunk = [0u8; CHUNK];

    loop {
        let read = card.read(file, &mut chunk)?;
        if read == 0 {
            break;
        }
        let pushed = buffer.push(&chunk[..read]);
        filled += pushed;
        if pushed < read {
            // The buffer is full. Whatever did not fit has already been read
            // from the card, so the caller winds the file back to `filled` —
            // otherwise those bytes are silently dropped from the middle of
            // the audio, which is a click rather than an error.
            break;
        }
    }
    Ok(filled)
}

fn describe(error: WavError) -> &'static str {
    match error {
        WavError::NotWave => "not a RIFF/WAVE file",
        WavError::Truncated => "the header is cut short",
        WavError::NotPcm => "not plain PCM samples",
        WavError::Malformed => "a chunk header is nonsense",
    }
}
