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

use core::sync::atomic::{AtomicBool, Ordering};

use embassy_futures::yield_now;
use embassy_time::{Duration, Instant};
use embedded_sdmmc::RawFile;
use esp_hal::dma::DmaTxStreamBuf;
use esp_hal::i2s::master::I2sTx;
use teddiebox_core::cushion::Cushion;
use teddiebox_core::wav::{WavError, WavFormat};

use teddiebox_audio::{LibOpus, OpusState, TafDecoder, MAX_FRAME_SAMPLES};

use crate::storage::{CardPages, Mounted};

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
    let Some((name, size)) = card.find_by_extension(b"WAV") else {
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

/// Decoder state and PCM scratch.
///
/// Static because neither belongs in an embassy task's future: the Opus state
/// alone is 28 KB, and a task arena sized to hold it would be paying for it on
/// every boot whether or not anything ever decodes.
struct DecodeScratch {
    opus: OpusState,
    pcm: [i16; MAX_FRAME_SAMPLES],
}

static mut SCRATCH: DecodeScratch = DecodeScratch {
    opus: OpusState::new(),
    pcm: [0; MAX_FRAME_SAMPLES],
};
static SCRATCH_TAKEN: AtomicBool = AtomicBool::new(false);

/// Hands out the decode scratch, once and once only.
///
/// The guard is what makes the `static mut` sound rather than merely
/// convention: a second caller gets `None` instead of a second `&mut` to the
/// same bytes.
fn take_scratch() -> Option<&'static mut DecodeScratch> {
    if SCRATCH_TAKEN.swap(true, Ordering::Relaxed) {
        return None;
    }
    // SAFETY: the swap above succeeds exactly once for the life of the
    // program, so no other reference to SCRATCH can exist.
    Some(unsafe { &mut *core::ptr::addr_of_mut!(SCRATCH) })
}

/// Reinterprets decoded samples as the bytes I2S wants.
///
/// The codec is configured for 16-bit little-endian samples and this is a
/// little-endian target, so the in-memory representation is already the wire
/// format. Converting sample by sample would be the same bytes, more slowly.
fn as_bytes(pcm: &[i16]) -> &[u8] {
    // SAFETY: `i16` has no padding and no invalid bit patterns, and any
    // alignment is valid for `u8`.
    unsafe { core::slice::from_raw_parts(pcm.as_ptr() as *const u8, core::mem::size_of_val(pcm)) }
}

/// Decodes the first `.TAF` on the card and plays it. Bench step 9.
///
/// Terminal, for the same reason as the others: the DMA buffer and the decode
/// scratch are each claimed once. `rb` restarts the box.
pub async fn play_first_taf(
    card: &Mounted,
    i2s_tx: I2sTx<'static, Blocking>,
    mut buffer: DmaTxStreamBuf,
) -> Result<(), &'static str> {
    let Some((name, size)) = card.find_by_extension(b"TAF") else {
        return Err("no .TAF in the card's root directory");
    };
    let scratch = take_scratch().ok_or("the decoder has already been used")?;

    esp_println::println!("teddiebox: taf {name}, {size} bytes");
    let file = card.open_file(name)?;
    let pages = CardPages::new(card, file, size);

    let opus = LibOpus::new(&mut scratch.opus).map_err(|_| "the Opus decoder would not start")?;
    let mut decoder = TafDecoder::open(pages, opus).map_err(|_| "not a readable TAF file")?;
    esp_println::println!(
        "teddiebox: taf audio id {:#010x}, {} chapters",
        decoder.header().audio_id,
        decoder.chapter_count()
    );

    // Pre-fill before the transfer starts, exactly as the WAV path does: a
    // buffer still empty when `write()` runs is played as silence and the
    // stream ends before the first sample arrives.
    let mut pending: core::ops::Range<usize> = 0..0;
    let mut ended = false;
    while !ended {
        if pending.is_empty() {
            match decoder.next_frame(&mut scratch.pcm) {
                Ok(Some(samples)) => pending = 0..samples * 2,
                Ok(None) => break,
                Err(_) => return Err("the first frames would not decode"),
            }
        }
        let pushed = buffer.push(&as_bytes(&scratch.pcm)[pending.clone()]);
        pending.start += pushed;
        if pushed == 0 {
            // The buffer is full; whatever is left stays pending for the
            // transfer loop below rather than being dropped.
            ended = true;
        }
    }

    let capacity = BUFFER_BYTES;
    let mut transfer = match i2s_tx.write(buffer) {
        Ok(transfer) => transfer,
        Err(_) => {
            card.close_file(file);
            return Err("I2S would not start");
        }
    };

    let mut cushion = Cushion::new(capacity as u32);
    cushion.start();
    let mut last_log = Instant::now();
    let started = Instant::now();
    let mut frames: u32 = 0;

    loop {
        cushion.observe(capacity.saturating_sub(transfer.available_bytes()) as u32);

        if pending.is_empty() {
            match decoder.next_frame(&mut scratch.pcm) {
                Ok(Some(samples)) => {
                    pending = 0..samples * 2;
                    frames += 1;
                }
                Ok(None) => break,
                Err(_) => {
                    esp_println::println!("teddiebox: taf decode failed after {frames} frames");
                    break;
                }
            }
        }

        let pushed = transfer.push(&as_bytes(&scratch.pcm)[pending.clone()]);
        pending.start += pushed;
        if pushed == 0 {
            yield_now().await;
        }

        if last_log.elapsed() >= LOG_EVERY {
            last_log = Instant::now();
            esp_println::println!(
                "teddiebox: taf {} s, {frames} frames, buffer {}% (low {}%), {} underruns",
                started.elapsed().as_secs(),
                cushion.percent(),
                cushion.low_water_percent(),
                cushion.underruns()
            );
        }

        yield_now().await;
    }

    while !transfer.is_done() {
        yield_now().await;
    }

    card.close_file(file);
    esp_println::println!(
        "teddiebox: taf done — {} s, {frames} frames, low water {}%, {} underruns",
        started.elapsed().as_secs(),
        cushion.low_water_percent(),
        cushion.underruns()
    );
    Ok(())
}
