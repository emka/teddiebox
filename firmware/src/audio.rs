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
use embassy_time::{Duration, Instant, Timer};
use embedded_sdmmc::RawFile;
use esp_hal::dma::DmaTxStreamBuf;
use esp_hal::i2s::master::I2sTx;
use teddiebox_core::checksum::Crc32;
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

/// How long to wait when the DMA buffer will not take any more.
///
/// The buffer drains at 192 KB/s, so a 4 KB chunk frees up roughly every
/// 21 ms — sleeping a millisecond costs nothing against that and avoids
/// spinning on `yield_now` for the whole window, which would burn the CPU on a
/// five-minute run for no benefit.
const BUFFER_FULL_WAIT: Duration = Duration::from_millis(1);

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
    let filled = prefill(card, file, &mut buffer, format.data_len as usize)?;
    // Prefill reads ahead of what it could fit, so put the file back exactly
    // where the buffer ends.
    card.seek(file, format.data_offset + filled as u32)?;
    esp_println::println!("teddiebox: wav buffer {filled} bytes pre-filled");

    // Saturating because a file shorter than the buffer is fully pre-filled,
    // and there is then nothing left to stream.
    let mut remaining = (format.data_len as usize).saturating_sub(filled);
    let mut transfer = match i2s_tx.write(buffer) {
        Ok(transfer) => transfer,
        Err(_) => {
            card.close_file(file);
            return Err("I2S would not start");
        }
    };

    // The denominator is the whole buffer, not what pre-fill happened to fit:
    // `available_bytes` reports free space across all of it.
    let mut cushion = Cushion::new(BUFFER_BYTES as u32);
    cushion.start();
    let mut chunk = [0u8; CHUNK];
    let mut last_log = Instant::now();
    let started = Instant::now();

    while remaining > 0 {
        let free = transfer.available_bytes();
        cushion.observe(BUFFER_BYTES.saturating_sub(free) as u32);

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
        } else {
            // Nothing would fit. Wait for the DMA to make room rather than
            // asking again immediately.
            Timer::after(BUFFER_FULL_WAIT).await;
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
    limit: usize,
) -> Result<usize, &'static str> {
    let mut filled = 0;
    let mut chunk = [0u8; CHUNK];

    loop {
        // Bounded by the data chunk: a WAV may carry metadata after its
        // samples, and reading into that would play it as the first audio out
        // of the speaker.
        let want = CHUNK.min(limit.saturating_sub(filled));
        if want == 0 {
            break;
        }
        let read = card.read(file, &mut chunk[..want])?;
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
    // Step 9's criterion is that the device's samples match the host's. The
    // box cannot hand back a WAV, so it checksums the PCM it decoded and the
    // host checksums what `taf2wav` produced from the same file — the same
    // trick step 7 used for the card, and the same CRC-32.
    //
    // Both of these start here rather than after the pre-fill: the frames that
    // fill the buffer are as much a part of the stream as the rest, and a
    // checksum that skipped them could never match.
    let mut pcm_crc = Crc32::new();
    let mut frames: u32 = 0;

    let mut pending: core::ops::Range<usize> = 0..0;
    let mut ended = false;
    while !ended {
        if pending.is_empty() {
            match decoder.next_frame(&mut scratch.pcm) {
                Ok(Some(samples)) => {
                    pcm_crc.update(as_bytes(&scratch.pcm[..samples]));
                    frames += 1;
                    pending = 0..samples * 2;
                }
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

    let mut transfer = match i2s_tx.write(buffer) {
        Ok(transfer) => transfer,
        Err(_) => {
            card.close_file(file);
            return Err("I2S would not start");
        }
    };

    let mut cushion = Cushion::new(BUFFER_BYTES as u32);
    cushion.start();
    let mut last_log = Instant::now();
    let started = Instant::now();
    // The other half of the criterion: how much of real time the decode costs.
    // Accumulated rather than sampled, because a mean hides exactly the frames
    // that would cause a dropout.
    let mut decode_us: u64 = 0;

    loop {
        cushion.observe(BUFFER_BYTES.saturating_sub(transfer.available_bytes()) as u32);

        if pending.is_empty() {
            let began = Instant::now();
            let decoded = decoder.next_frame(&mut scratch.pcm);
            decode_us += began.elapsed().as_micros();

            match decoded {
                Ok(Some(samples)) => {
                    pcm_crc.update(as_bytes(&scratch.pcm[..samples]));
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
            // The buffer is full and a frame is waiting. Let the DMA drain.
            Timer::after(BUFFER_FULL_WAIT).await;
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

    let elapsed = started.elapsed();
    esp_println::println!(
        "teddiebox: taf done — {} s, {frames} frames, low water {}%, {} underruns",
        elapsed.as_secs(),
        cushion.low_water_percent(),
        cushion.underruns()
    );
    esp_println::println!("teddiebox: taf pcm crc32 {:08X}", pcm_crc.finish());

    // Decode cost as a percentage of the audio it produced. Under 100 means
    // the box can decode faster than it plays, and the margin is the headroom
    // step 9 asks to be measured rather than assumed.
    let played_us = elapsed.as_micros().max(1);
    esp_println::println!(
        "teddiebox: taf decode used {decode_us} us of {played_us} us — {}% of real time",
        decode_us * 100 / played_us
    );
    Ok(())
}
