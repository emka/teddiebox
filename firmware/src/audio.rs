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

use core::sync::atomic::{AtomicBool, AtomicI8, AtomicU16, AtomicU32};
use portable_atomic::Ordering;

use embassy_futures::yield_now;
use embassy_time::{Duration, Instant, Timer};
use embedded_sdmmc::RawFile;
use esp_hal::dma::DmaTxStreamBuf;
use esp_hal::i2s::master::I2sTx;
use teddiebox_core::checksum::Crc32;
use teddiebox_core::cushion::Cushion;
use teddiebox_core::wav::{WavError, WavFormat};
use teddiebox_core::Position;

use teddiebox_audio::{LibOpus, OpusState, Skip, TafDecoder, MAX_FRAME_SAMPLES};

use crate::storage::{CardPages, Mounted, PAGE_READ_MAX_US, PAGE_READ_US};

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

/// How many consecutive full-buffer waits mean something is wrong rather than
/// busy. At one millisecond each, this is a second of a buffer that holds
/// about 170 ms.
const STALL_REPORT_AFTER: u32 = 1000;

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
/// Set to ask whatever is playing to stop at the next frame.
///
/// Read by the playback loop rather than acted on from outside it: the DMA
/// transfer owns the I2S transmitter and the buffer, and only the loop that
/// created it can hand them back.
pub static STOP: AtomicBool = AtomicBool::new(false);

/// The container page the decoder is on, and the chapter it is in.
///
/// Position memory reads both from outside this module: the page for the exact
/// tier it keeps in RAM, the chapter for the one it writes to the card. The
/// decoder itself is created inside the playback loop and is reachable from
/// nowhere else, which is the same reason `STOP` and `SKIP` are statics.
pub static PAGE: AtomicU32 = AtomicU32::new(0);
pub static CHAPTER: AtomicU16 = AtomicU16::new(0);

/// How playback ended, which decides whether a saved position is worth
/// keeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finish {
    /// The stream ran out. A finished story is not a paused one, so whoever
    /// owns the position clears it.
    Ended,
    /// `STOP` was set — a figure lifted, a console `stop`, a new request.
    Stopped,
}

/// Set to ask whatever is playing to skip a chapter at the next frame:
/// [`SKIP_FORWARD`] or [`SKIP_BACK`].
///
/// Read by the playback loop for the same reason [`STOP`] is — the decoder is
/// created inside it and reachable from nowhere else — and set from outside by
/// whoever decided a chapter should change. Which is the same bargain
/// `on_frame` strikes: this module is told what to do and never learns that an
/// ear exists.
///
/// The last request wins rather than accumulating. Recognising a held ear
/// takes longer than half a second and a frame is 60 ms, so the loop has
/// always read one before the next can arrive.
pub static SKIP: AtomicI8 = AtomicI8::new(SKIP_NONE);

pub const SKIP_NONE: i8 = 0;
pub const SKIP_FORWARD: i8 = 1;
pub const SKIP_BACK: i8 = -1;

/// Hands the scratch back, so another playback can have it.
///
/// Playback used to be terminal — the decoder was claimed once for the life of
/// the program — which made `stop` meaningless and a second `taf` impossible.
fn release_scratch() {
    SCRATCH_TAKEN.store(false, Ordering::Relaxed);
}

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
/// Decodes `frames` frames of the first TAF and prints the samples.
///
/// No I2S, no playback, no real-time constraint: step 9 asks whether the box
/// decodes the same audio the host does, and that is a question about numbers
/// rather than about sound. A checksum can only answer yes or no, and Opus is
/// not specified to be bit-exact across platforms — so the samples themselves
/// are what the host needs in order to say *how far apart*.
///
/// Hex rather than anything denser because the console is a text stream shared
/// with the box's own logging, and a decoder that has to be told where the
/// data starts is one more thing to get wrong.
pub async fn dump_pcm(card: &Mounted, frames: u8) -> Result<(), &'static str> {
    let Some(scratch) = take_scratch() else {
        return Err("the decoder is already in use");
    };

    let (file, size) = match card.find_by_extension(b"TAF") {
        Some((name, size)) => {
            esp_println::println!("teddiebox: pcm /{name}, {size} bytes");
            (card.open_file(name)?, size)
        }
        None => {
            release_scratch();
            return Err("no .TAF in the root to decode");
        }
    };

    let pages = CardPages::new(card, file, size);
    let result = (|| {
        let opus =
            LibOpus::new(&mut scratch.opus).map_err(|_| "the Opus decoder would not start")?;
        let mut decoder = TafDecoder::open(pages, opus).map_err(|_| "not a readable TAF file")?;
        let mut crc = Crc32::new();
        for index in 0..frames {
            match decoder.next_frame(&mut scratch.pcm) {
                Ok(Some(samples)) => {
                    crc.update(as_bytes(&scratch.pcm[..samples]));
                    // One line per frame, so a divergence can be placed as
                    // well as detected.
                    esp_println::print!("teddiebox: pcm {index} ");
                    for sample in &scratch.pcm[..samples] {
                        esp_println::print!("{:04X}", *sample as u16);
                    }
                    esp_println::println!();
                }
                Ok(None) => break,
                Err(_) => return Err("a frame would not decode"),
            }
        }
        esp_println::println!("teddiebox: pcm crc32 {:08X}", crc.finish());
        Ok(())
    })();

    card.close_file(file);
    release_scratch();
    result
}

/// Which audio file a playback should decode.
pub enum Source {
    /// Whatever the card offers: a `.TAF` in the root if there is one, else
    /// the first Tonie under `CONTENT/`.
    First,
    /// One named `CONTENT/<directory>/<file>`.
    Content { directory: u32, file: u32 },
    /// One a download put in `CACHE/<directory>/<file>`.
    Cache { directory: u32, file: u32 },
}

/// Plays one TAF, handing back the hardware it borrowed.
///
/// Returning the I2S transmitter and the DMA buffer is what makes playback
/// something the box can do twice. It used to be terminal — `rb` between every
/// attempt — which also made stopping meaningless, since nothing could follow.
///
/// `on_frame` is called once per pass of the decode loop. Playback owns the
/// task for the length of a story, so this is the caller's only opportunity to
/// do anything at all while one plays; it exists so that whoever started the
/// story can keep watching for a reason to end it. This module deliberately
/// knows nothing about what that reason might be — it hands over a turn and
/// then reads [`STOP`], exactly as it already did for the console.
pub async fn play_taf(
    card: &Mounted,
    i2s_tx: I2sTx<'static, Blocking>,
    buffer: DmaTxStreamBuf,
    source: Source,
    from: Position,
    on_frame: &mut dyn FnMut(),
) -> (
    Result<Finish, &'static str>,
    I2sTx<'static, Blocking>,
    DmaTxStreamBuf,
) {
    match play_taf_inner(card, i2s_tx, buffer, source, from, on_frame).await {
        Ok((finish, tx, buffer)) => (Ok(finish), tx, buffer),
        Err((reason, tx, buffer)) => (Err(reason), tx, buffer),
    }
}

type Reclaimed = (Finish, I2sTx<'static, Blocking>, DmaTxStreamBuf);
type PlaybackError = (&'static str, I2sTx<'static, Blocking>, DmaTxStreamBuf);

async fn play_taf_inner(
    card: &Mounted,
    i2s_tx: I2sTx<'static, Blocking>,
    mut buffer: DmaTxStreamBuf,
    source: Source,
    from: Position,
    on_frame: &mut dyn FnMut(),
) -> Result<Reclaimed, PlaybackError> {
    let Some(scratch) = take_scratch() else {
        return Err(("the decoder is already in use", i2s_tx, buffer));
    };

    // A `.TAF` copied into the root wins, so a specific file can be chosen for
    // a test. Failing that, the card's own content is used where the Toniebox
    // keeps it — which means step 9 runs against the stock card without
    // anything being written to it.
    let opened = match source {
        Source::Content { directory, file } => card.open_content(directory, file),
        Source::Cache { directory, file } => card.open_cache(directory, file),
        Source::First => match card.find_by_extension(b"TAF") {
            Some((name, size)) => {
                esp_println::println!("teddiebox: taf /{name}, {size} bytes");
                card.open_file(name).map(|handle| (handle, size))
            }
            None => card
                .open_first_tonie()
                .ok_or("no .TAF in the root and no CONTENT/<hex>/500304E0 on the card"),
        },
    };
    let (file, size) = match opened {
        Ok(pair) => pair,
        Err(reason) => {
            release_scratch();
            return Err((reason, i2s_tx, buffer));
        }
    };
    esp_println::println!("teddiebox: taf {size} bytes");
    let pages = CardPages::new(card, file, size);

    let opus = match LibOpus::new(&mut scratch.opus) {
        Ok(opus) => opus,
        Err(_) => {
            card.close_file(file);
            release_scratch();
            return Err(("the Opus decoder would not start", i2s_tx, buffer));
        }
    };
    let mut decoder = match TafDecoder::open(pages, opus) {
        Ok(decoder) => decoder,
        Err(_) => {
            card.close_file(file);
            release_scratch();
            return Err(("not a readable TAF file", i2s_tx, buffer));
        }
    };
    esp_println::println!(
        "teddiebox: taf audio id {:#010x}, {} chapters",
        decoder.header().audio_id,
        decoder.chapter_count()
    );

    // Where to start, and what to do when it cannot be honoured. A story that
    // has been replaced or re-downloaded can name a page or a chapter it no
    // longer has, and that is a reason to play it from the beginning — never a
    // reason to refuse to play it.
    match from {
        Position::Start => {}
        Position::Chapter(n) => {
            if decoder.seek_to_chapter(n as usize).is_err() {
                esp_println::println!(
                    "teddiebox: taf chapter {n} is not in this story — starting at the top"
                );
            } else {
                esp_println::println!("teddiebox: taf resuming at chapter {n}");
            }
        }
        Position::Exact { page } => {
            if decoder.seek_to_page(page).is_err() {
                esp_println::println!(
                    "teddiebox: taf page {page} is not in this story — starting at the top"
                );
            } else {
                esp_println::println!("teddiebox: taf resuming at page {page}");
            }
        }
    }

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
                Err(_) => {
                    card.close_file(file);
                    release_scratch();
                    return Err(("the first frames would not decode", i2s_tx, buffer));
                }
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
        Err((_, tx, buffer)) => {
            card.close_file(file);
            release_scratch();
            return Err(("I2S would not start", tx, buffer));
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

    // The quantity that decides everything: how long the loop is ever away.
    // The DMA stops if it drains the queued descriptors before this loop links
    // another one on, so "how late does the media task get" is the whole
    // question and every other explanation has been a proxy for it.
    let mut longest_gap_us: u64 = 0;
    let mut last_pass = Instant::now();
    let mut stopped = false;
    // How many times the DMA ran off the end of its descriptor chain and had
    // to be started again. Zero is the only good number; anything else is
    // audible.
    let mut restarts: u32 = 0;
    // Consecutive milliseconds the buffer has refused a byte.
    let mut stalled: u32 = 0;
    loop {
        // The caller's turn. This loop is the whole media task for as long as
        // a story lasts, so anything that decides playback should end — a
        // figure lifted off the plate above all — can only be noticed from in
        // here. It is given the turn *before* the stop check, so a decision
        // made now is acted on now rather than one frame later.
        on_frame();

        if STOP.swap(false, Ordering::Relaxed) {
            stopped = true;
            break;
        }

        // Read after the stop, so an ear still held as a figure is lifted does
        // not skip within a story the box is about to stop playing anyway.
        //
        // A skip drops the frame half-pushed into the buffer, or the tail of
        // the chapter just left would be played over the start of the new one.
        // What the DMA already holds — up to `BUFFER_BYTES` — is not
        // recoverable, so a little of the old chapter is still heard. That is
        // a chosen cost: draining it would mean stopping and restarting the
        // transfer, which is the thing this box clicks through.
        //
        // A run that skipped has a `pcm_crc` that no host decode of the whole
        // file can match. That is correct — it is not the same audio — and
        // step 9's comparison simply must not skip.
        match SKIP.swap(SKIP_NONE, Ordering::Relaxed) {
            SKIP_NONE => {}
            n if n > 0 => match decoder.next_chapter() {
                Ok(Skip::To(chapter)) => {
                    esp_println::println!("teddiebox: taf skipped to chapter {chapter}");
                    pending = 0..0;
                }
                // Skipping out of the last chapter is the end of the story,
                // which is the same thing the stream running out means.
                Ok(Skip::PastTheEnd) => {
                    esp_println::println!("teddiebox: taf skipped past the last chapter");
                    break;
                }
                Err(_) => esp_println::println!("teddiebox: taf could not skip forward"),
            },
            _ => match decoder.previous_chapter() {
                Ok(chapter) => {
                    esp_println::println!("teddiebox: taf skipped back to chapter {chapter}");
                    pending = 0..0;
                }
                Err(_) => esp_println::println!("teddiebox: taf could not skip back"),
            },
        }
        // Checked before the buffer level, because the level cannot tell this
        // apart. `available_bytes` counts descriptors the CPU owns, so zero
        // means either "full" or "the DMA has stopped owning nothing back" —
        // and `DmaTxStreamBuf` stops for good when its chain runs dry. Read as
        // fullness it produces a loop that waits for ever on a buffer that is
        // actually empty and dead, reporting 100% and no underruns while a
        // child hears silence.
        // The chain has ended. `DmaTxStreamBuf` links each filled descriptor
        // onto a list that always terminates in a null `next`, so if the DMA
        // reaches that tail before the CPU appends the next one it runs off the
        // end and *stops* — and writing `prev.next` afterwards does not revive
        // it. Every append is a chance to lose that race, which is why load
        // makes it likelier and why it happens with the buffer still two-thirds
        // full rather than empty.
        //
        // So this is not something to be fast enough to avoid. Restart the
        // transfer and carry on: a gap of a few milliseconds is a glitch, and
        // the alternative is silence for the rest of the story.
        if transfer.is_done() {
            restarts += 1;
            // *When* matters. Restarts bunched at the first frame are a
            // priming problem and fixable; ones spread through playback are
            // the descriptor race and are not.
            esp_println::println!(
                "teddiebox: taf restart {restarts} at frame {frames}, {} ms in",
                started.elapsed().as_millis()
            );
            let (tx, buf) = transfer.stop();
            transfer = match tx.write(buf) {
                Ok(started) => started,
                Err((_, tx, buffer)) => {
                    esp_println::println!(
                        "teddiebox: taf could not restart the DMA after {frames} frames"
                    );
                    card.close_file(file);
                    release_scratch();
                    return Err(("the DMA would not restart", tx, buffer));
                }
            };
            continue;
        }
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
                    // Published for position memory, which cannot reach the
                    // decoder: it lives inside this loop and nowhere else.
                    PAGE.store(decoder.page(), Ordering::Relaxed);
                    CHAPTER.store(decoder.chapter() as u16, Ordering::Relaxed);
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
            stalled += 1;
            // A full buffer is normal for a millisecond. A full buffer for a
            // second means nothing is draining it, and "full" is then a lie:
            // `available_bytes` counts descriptors owned by the CPU, so a DMA
            // that has stopped owns them all and reads as full rather than as
            // dead. Say which it is, once.
            if stalled == STALL_REPORT_AFTER {
                esp_println::println!(
                    "teddiebox: taf buffer has not drained in {} ms — dma done {}, available {}",
                    STALL_REPORT_AFTER,
                    transfer.is_done(),
                    transfer.available_bytes()
                );
            }
        } else {
            stalled = 0;
        }

        if last_log.elapsed() >= LOG_EVERY {
            last_log = Instant::now();
            let so_far = started.elapsed().as_micros().max(1);
            // The checksum and the decode cost so far, not only at the end.
            // A whole Tonie is over half an hour of loud audio in the room,
            // and every figure step 9 wants is comparable at any frame count:
            // `taf2wav --frames N` decodes exactly this many on the host.
            esp_println::println!(
                "teddiebox: taf {} s, {frames} frames, buffer {}% (low {}%), {} underruns,                  crc32 {:08X}, decode {}%",
                so_far / 1_000_000,
                cushion.percent(),
                cushion.low_water_percent(),
                cushion.underruns(),
                pcm_crc.finish(),
                decode_us * 100 / so_far
            );
            // `available_bytes` counts descriptors the CPU owns, so a DMA that
            // has stopped reads as a *full* buffer rather than an empty one —
            // which is why a stall reports 100% and no underruns. These two
            // say which it really is.
            esp_println::println!(
                "teddiebox: taf   dma done {}, available {} bytes, pending {}",
                transfer.is_done(),
                transfer.available_bytes(),
                pending.len()
            );
            let card_us = PAGE_READ_US.load(Ordering::Relaxed);
            esp_println::println!(
                "teddiebox: taf   longest card read {} ms, longest loop gap {} ms",
                PAGE_READ_MAX_US.load(Ordering::Relaxed) / 1000,
                longest_gap_us / 1000
            );
            esp_println::println!(
                "teddiebox: taf   of that, card reads {}% and decode {}%",
                card_us * 100 / so_far,
                decode_us.saturating_sub(card_us) * 100 / so_far
            );
        }

        yield_now().await;
        let gap = last_pass.elapsed().as_micros();
        if gap > longest_gap_us {
            longest_gap_us = gap;
        }
        last_pass = Instant::now();
    }

    // A stop must not wait for the buffer to drain — that is most of a second
    // of audio after the word was typed, which does not read as having
    // stopped.
    if !stopped {
        while !transfer.is_done() {
            yield_now().await;
        }
    }
    let (i2s_tx, buffer) = transfer.stop();

    card.close_file(file);
    release_scratch();

    let elapsed = started.elapsed();
    if stopped {
        esp_println::println!(
            "teddiebox: taf stopped after {frames} frames, {} s",
            elapsed.as_secs()
        );
    }
    esp_println::println!(
        // The underrun count comes from the cushion, which watches the buffer
        // level and cannot see this one: a stopped DMA reads as a full buffer,
        // so the level never reaches zero. Reported alongside rather than
        // folded in, so the cushion keeps meaning what it measures.
        "teddiebox: taf done — {} s, {frames} frames, low water {}%, {} underruns, {} dma restarts",
        elapsed.as_secs(),
        cushion.low_water_percent(),
        cushion.underruns(),
        restarts
    );
    esp_println::println!("teddiebox: taf pcm crc32 {:08X}", pcm_crc.finish());

    // Decode cost as a percentage of the audio it produced. Under 100 means
    // the box can decode faster than it plays, and the margin is the headroom
    // step 9 asks to be measured rather than assumed.
    let played_us = elapsed.as_micros().max(1);
    let card_us = PAGE_READ_US.load(Ordering::Relaxed);
    esp_println::println!(
        "teddiebox: taf feeding the codec used {}% of real time — card reads {}%, decode {}%",
        decode_us * 100 / played_us,
        card_us * 100 / played_us,
        decode_us.saturating_sub(card_us) * 100 / played_us
    );
    Ok((
        if stopped {
            Finish::Stopped
        } else {
            Finish::Ended
        },
        i2s_tx,
        buffer,
    ))
}
