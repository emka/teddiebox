//! Audio playback: WAV files (for testing) and TAF stories, from the card to
//! the codec over I2S.
//!
//! The WAV path has no decoder, so it tests only whether the card can supply
//! bytes fast enough; an underrun cannot be confused with a decode fault.
//!
//! Buffer accounting ([`teddiebox_core::cushion`]) and WAV header parsing
//! ([`teddiebox_core::wav`]) are tested on the host. This file drives the DMA.

use core::sync::atomic::{AtomicBool, AtomicI8, AtomicU32, AtomicU8};
use portable_atomic::Ordering;

use embassy_futures::yield_now;
use embassy_time::{Duration, Instant, Timer};
use embedded_sdmmc::RawFile;
use esp_hal::dma::DmaTxStreamBuf;
use esp_hal::i2s::master::I2sTx;
use teddiebox_core::checksum::Crc32;
use teddiebox_core::cue::{Cue, CueSamples};
use teddiebox_core::cushion::Cushion;
use teddiebox_core::wav::{WavError, WavFormat};
use teddiebox_core::Position;

use teddiebox_audio::{LibOpus, OpusState, Skip, TafDecoder, MAX_FRAME_SAMPLES};

use crate::storage::{CardPages, Mounted, PAGE_READ_MAX_US, PAGE_READ_US};

/// The DMA buffer between the card and the codec.
///
/// About 170 ms at 48 kHz stereo 16-bit. The low-water mark printed during
/// playback shows whether it is big enough.
pub const BUFFER_BYTES: usize = 32_768;

/// Bytes read from the card in one go.
///
/// A whole number of 512-byte blocks, large enough that each read's fixed
/// cost is small in comparison.
const CHUNK: usize = 4096;

/// Samples of a cue fed to the DMA at a time: 20 ms of stereo.
const CUE_CHUNK: usize = 960 * 2;

/// How often the occupancy line is printed.
const LOG_EVERY: Duration = Duration::from_secs(5);

/// How long to wait when the DMA buffer will not take any more.
///
/// The buffer drains at 192 KB/s, so 4 KB frees up about every 21 ms.
/// Sleeping 1 ms avoids spinning the CPU on `yield_now` meanwhile.
const BUFFER_FULL_WAIT: Duration = Duration::from_millis(1);

/// How many full-buffer waits in a row mean something is wrong: one second,
/// for a buffer that holds about 170 ms.
const STALL_REPORT_AFTER: u32 = 1000;

type Blocking = esp_hal::Blocking;

/// Plays the first `.WAV` in the card's root directory.
///
/// Can only run once per boot, like the test tone: it takes the DMA buffer
/// for good. Use `rb` to play it again.
pub async fn play_first_wav(
    card: &Mounted,
    i2s_tx: I2sTx<'static, Blocking>,
    buffer: DmaTxStreamBuf,
) -> Result<(), &'static str> {
    let mut buffer = emptied(buffer);
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

    // I2S and the codec are configured once at boot. A file in another format
    // would play at the wrong speed instead of failing.
    if !format.matches_codec() {
        card.close_file(file);
        return Err("the file is not 48 kHz stereo 16-bit");
    }

    card.seek(file, format.data_offset)?;

    // Fill the buffer before starting: `write()` starts the transfer at once,
    // and an empty buffer would run out before the first sample arrived.
    let filled = prefill(card, file, &mut buffer, format.data_len as usize)?;
    // Prefill may read more than fits, so seek back to where the buffer ends.
    card.seek(file, format.data_offset + filled as u32)?;
    esp_println::println!("teddiebox: wav buffer {filled} bytes pre-filled");

    // A file shorter than the buffer is completely pre-filled.
    let mut remaining = (format.data_len as usize).saturating_sub(filled);
    let mut transfer = match i2s_tx.write(buffer) {
        Ok(transfer) => transfer,
        Err(_) => {
            card.close_file(file);
            return Err("I2S would not start");
        }
    };

    // Measured against the whole buffer: `available_bytes` reports free
    // space across all of it.
    let mut cushion = Cushion::new(BUFFER_BYTES as u32);
    cushion.start();
    let mut chunk = [0u8; CHUNK];
    let mut last_log = Instant::now();
    let started = Instant::now();

    while remaining > 0 {
        let free = transfer.available_bytes();
        cushion.observe(BUFFER_BYTES.saturating_sub(free) as u32);

        let want = CHUNK.min(remaining);
        if free >= want {
            let read = card.read(file, &mut chunk[..want])?;
            if read == 0 {
                // The file is shorter than its header says. Not fatal.
                esp_println::println!("teddiebox: wav ended {remaining} bytes early");
                break;
            }

            // `push` only takes what fits, so push the rest again.
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
            // Nothing fits. Wait for the DMA to make room.
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

/// A used stream buffer, made ready for a new transfer with nothing in it.
///
/// `DmaTxStreamBuf` counts the bytes pushed before a transfer from its
/// creation and never resets the count. On any transfer after the first,
/// `push` therefore appends after the previous transfer's bytes, and the
/// transfer opens by playing whatever the buffer last held: up to
/// `BUFFER_BYTES` of the previous story or sound. Rebuilding it from its own
/// parts starts the count at zero.
fn emptied(buffer: DmaTxStreamBuf) -> DmaTxStreamBuf {
    let (descriptors, memory) = buffer.split();
    DmaTxStreamBuf::new(descriptors, memory).expect("the parts it was built from at boot")
}

/// Fills the DMA buffer from the card before the transfer starts.
///
/// Returns how many bytes went in.
fn prefill(
    card: &Mounted,
    file: RawFile,
    buffer: &mut DmaTxStreamBuf,
    limit: usize,
) -> Result<usize, &'static str> {
    let mut filled = 0;
    let mut chunk = [0u8; CHUNK];

    loop {
        // Stop at the end of the data chunk: a WAV may have metadata after
        // its samples, which must not be played.
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
            // The buffer is full. The part that did not fit was already read,
            // so the caller seeks back to `filled`; otherwise those bytes
            // would be lost, causing a click.
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
/// Static rather than in an embassy task's future: the Opus state alone is
/// 28 KB.
struct DecodeScratch {
    opus: OpusState,
    pcm: [i16; MAX_FRAME_SAMPLES],
}

static mut SCRATCH: DecodeScratch = DecodeScratch {
    opus: OpusState::new(),
    pcm: [0; MAX_FRAME_SAMPLES],
};
static SCRATCH_TAKEN: AtomicBool = AtomicBool::new(false);

/// The scratch measured as plain memory, for whoever borrows it wholesale.
///
/// See [`take_scratch_bytes`]. The assertion below checks that
/// `DecodeScratch` has no padding, so every byte is initialised and viewing
/// it as bytes is sound. A new field that adds padding breaks the build.
pub const SCRATCH_BYTES: usize = core::mem::size_of::<DecodeScratch>();
const _: () = assert!(
    SCRATCH_BYTES
        == core::mem::size_of::<OpusState>() + core::mem::size_of::<[i16; MAX_FRAME_SAMPLES]>(),
    "DecodeScratch has padding, so its bytes are not all initialised"
);

/// Set to ask whatever is playing to stop at the next frame.
///
/// Read by the playback loop, because only that loop can hand back the I2S
/// transmitter and buffer the DMA transfer owns.
pub static STOP: AtomicBool = AtomicBool::new(false);

/// The container page the decoder is on: the position that is saved when a
/// figure is lifted.
///
/// A static, because the decoder only exists inside the playback loop (the
/// same reason as for `STOP` and `SKIP`).
pub static PAGE: AtomicU32 = AtomicU32::new(0);

/// How playback ended, which decides whether a saved position is worth
/// keeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finish {
    /// The story played to the end, so its saved position is cleared.
    Ended,
    /// `STOP` was set: a figure lifted, a console `stop`, or a new request.
    Stopped,
}

/// Set to ask whatever is playing to skip a chapter at the next frame:
/// [`SKIP_FORWARD`] or [`SKIP_BACK`].
///
/// Read by the playback loop, like [`STOP`], and set by whoever decides a
/// chapter should change. This module does not know about ears.
///
/// The last request wins; requests do not add up. A held ear takes over half
/// a second to recognise and a frame is 60 ms, so each request is read before
/// the next arrives.
pub static SKIP: AtomicI8 = AtomicI8::new(SKIP_NONE);

pub const SKIP_NONE: i8 = 0;
pub const SKIP_FORWARD: i8 = 1;
pub const SKIP_BACK: i8 = -1;

/// A cue waiting to sound: [`CUE_NONE`], or a [`Cue::code`].
///
/// Set by whoever decides a press deserves a cue; taken by whatever holds
/// the I2S output. A story's loop mixes it in; the idle media loop plays it
/// on its own. The last request wins, as for [`SKIP`].
pub static CUE: AtomicU8 = AtomicU8::new(CUE_NONE);

pub const CUE_NONE: u8 = 0;

/// Takes the waiting cue, if there is one.
pub fn take_cue() -> Option<Cue> {
    Cue::from_code(CUE.swap(CUE_NONE, Ordering::Relaxed))
}

/// Silence after an idle cue, so the transfer never stops mid-tone: 20 ms.
const CUE_TAIL_FRAMES: usize = 960;

/// Plays a cue when no story holds the output.
///
/// The output must already be powered. Gives the transmitter and buffer
/// back whatever happens, so a story can play afterwards.
pub async fn play_cue(
    i2s_tx: I2sTx<'static, Blocking>,
    buffer: DmaTxStreamBuf,
    cue: Cue,
) -> (
    Result<(), &'static str>,
    I2sTx<'static, Blocking>,
    DmaTxStreamBuf,
) {
    let mut buffer = emptied(buffer);
    let mut samples = cue.samples();
    let mut chunk = [0i16; CUE_CHUNK];
    let mut pending: core::ops::Range<usize> = 0..0;
    // `black_box` keeps the tail out of constant folding: the Xtensa backend
    // cannot select the `add 3840` it would fold into ("Cannot select:
    // Constant<3840>").
    let mut left = (cue.frames() as usize + core::hint::black_box(CUE_TAIL_FRAMES)) * 4;

    // Filled before starting, as a story is: a stream transfer starts at
    // once and would run dry before the first sample arrived.
    loop {
        refill(&mut samples, &mut chunk, &mut pending, left);
        if pending.is_empty() {
            break;
        }
        let pushed = buffer.push(&as_bytes(&chunk)[pending.clone()]);
        pending.start += pushed;
        left -= pushed;
        if pushed == 0 {
            break;
        }
    }

    let mut transfer = match i2s_tx.write(buffer) {
        Ok(transfer) => transfer,
        Err((_, tx, buffer)) => return (Err("I2S would not start"), tx, buffer),
    };
    while left > 0 {
        refill(&mut samples, &mut chunk, &mut pending, left);
        let pushed = transfer.push(&as_bytes(&chunk)[pending.clone()]);
        pending.start += pushed;
        left -= pushed;
        if pushed == 0 {
            Timer::after(BUFFER_FULL_WAIT).await;
        }
    }
    // Everything is queued; the DMA stops by itself at the end of it.
    let deadline = Instant::now() + Duration::from_millis(u64::from(cue.frames()) / 48 + 200);
    while !transfer.is_done() && Instant::now() < deadline {
        Timer::after(Duration::from_millis(1)).await;
    }
    let (tx, buffer) = transfer.stop();
    (Ok(()), tx, buffer)
}

/// Refills `chunk` from the cue once the last one has all been pushed.
fn refill(
    samples: &mut CueSamples,
    chunk: &mut [i16; CUE_CHUNK],
    pending: &mut core::ops::Range<usize>,
    left: usize,
) {
    if core::ops::Range::is_empty(pending) && left > 0 {
        samples.fill(chunk);
        *pending = 0..(CUE_CHUNK * 2).min(left);
    }
}

/// Hands the scratch back, so another playback can use it.
fn release_scratch() {
    SCRATCH_TAKEN.store(false, Ordering::Relaxed);
}

/// Hands out the decode scratch, to one user at a time.
///
/// The flag makes the `static mut` safe: a second caller gets `None` instead
/// of a second `&mut` to the same bytes.
fn take_scratch() -> Option<&'static mut DecodeScratch> {
    if SCRATCH_TAKEN.swap(true, Ordering::Relaxed) {
        return None;
    }
    // SAFETY: the swap succeeds only while no one else holds the scratch (it
    // is reset by `release_scratch`), so no other reference to SCRATCH
    // exists.
    Some(unsafe { &mut *core::ptr::addr_of_mut!(SCRATCH) })
}

/// Hands the same scratch out as plain bytes, once and for good.
///
/// Used by the setup portal, which needs room for its future (socket buffers
/// and the radio's `StackResources`) and never decodes. Reusing this 51,712
/// byte buffer keeps the portal out of `.bss`, which would shrink the stack.
/// See `portal.rs`.
///
/// Uses the same [`SCRATCH_TAKEN`] flag as [`take_scratch`], so whichever
/// comes second gets `None`.
///
/// **Never released**: the portal never returns (it resets or stops the box).
pub fn take_scratch_bytes() -> Option<&'static mut [u8]> {
    if SCRATCH_TAKEN.swap(true, Ordering::Relaxed) {
        return None;
    }
    // SAFETY: the swap succeeds only if no one holds the scratch, and it is
    // never released afterwards, so no other reference to SCRATCH exists. The
    // region is SCRATCH_BYTES long and fully initialised (no padding, checked
    // above), and `u8` accepts any bit pattern and alignment.
    Some(unsafe {
        core::slice::from_raw_parts_mut(
            core::ptr::addr_of_mut!(SCRATCH).cast::<u8>(),
            SCRATCH_BYTES,
        )
    })
}

/// Reinterprets decoded samples as the bytes I2S wants.
///
/// The codec takes 16-bit little-endian samples and this chip is
/// little-endian, so the samples in memory are already in the right format.
fn as_bytes(pcm: &[i16]) -> &[u8] {
    // SAFETY: `i16` has no padding and no invalid bit patterns, and any
    // alignment is valid for `u8`.
    unsafe { core::slice::from_raw_parts(pcm.as_ptr() as *const u8, core::mem::size_of_val(pcm)) }
}

/// Decodes `frames` frames of the first TAF and prints the samples.
///
/// No playback: this compares the box's decoded samples with the host's.
/// Opus is not guaranteed bit-exact across platforms, so the samples are
/// printed to see how far apart they are, not just a checksum.
///
/// Hex, because the console is a text stream shared with the box's logging.
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
                    // One line per frame, to see where a difference starts.
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
/// Returns the I2S transmitter and DMA buffer, so playback can run again.
///
/// `on_frame` is called once per pass of the decode loop. Playback runs in
/// the caller's task for the whole story, so this is the caller's only chance
/// to do anything meanwhile, such as noticing the figure was lifted. This
/// module does not know why; it just checks [`STOP`] afterwards.
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
    buffer: DmaTxStreamBuf,
    source: Source,
    from: Position,
    on_frame: &mut dyn FnMut(),
) -> Result<Reclaimed, PlaybackError> {
    let mut buffer = emptied(buffer);
    let Some(scratch) = take_scratch() else {
        return Err(("the decoder is already in use", i2s_tx, buffer));
    };

    // A `.TAF` in the root is used first, so a specific file can be tested.
    // Otherwise the card's own content is used, so nothing has to be written
    // to the card.
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

    // Where to start. A replaced or re-downloaded story may not have the saved
    // page any more; then it plays from the beginning.
    match from {
        Position::Start => {}
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

    // Fill the buffer before starting, as the WAV path does: an empty buffer
    // would run out before the first sample arrived.
    //
    // A CRC-32 of the decoded samples, to compare with `taf2wav` output on the
    // host. It includes the pre-fill frames.
    let mut pcm_crc = Crc32::new();
    let mut frames: u32 = 0;

    let mut pending: core::ops::Range<usize> = 0..0;
    loop {
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
            // The buffer is full; the rest stays pending for the loop below.
            break;
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
    // Total decode time, to compare with real time.
    let mut decode_us: u64 = 0;

    // The longest time between loop passes. The DMA stops if it runs through
    // its queued descriptors before the loop adds another, so this is what
    // matters most.
    let mut longest_gap_us: u64 = 0;
    let mut last_pass = Instant::now();
    let mut stopped = false;
    // How many times the DMA ran off the end of its descriptor chain and had
    // to be restarted. Each one is audible.
    let mut restarts: u32 = 0;
    // Consecutive milliseconds the buffer has refused a byte.
    let mut stalled: u32 = 0;
    // A cue sounding over the story, mixed into each frame as it is decoded.
    let mut over: Option<CueSamples> = None;
    // A cue that takes the story's place for a moment, as a skip's does.
    // While it lasts, frames come from it instead of the decoder.
    let mut instead: Option<CueSamples> = None;
    // Set by a skip past the last chapter: the story ends once its cue has
    // played.
    let mut end_after_cue = false;
    loop {
        // The caller's turn, since this loop occupies the media task for the
        // whole story (for example to notice a lifted figure). Called before
        // the stop check, so its decision takes effect at once.
        on_frame();

        if STOP.swap(false, Ordering::Relaxed) {
            stopped = true;
            break;
        }

        // Checked after the stop, so a skip does not happen in a story that
        // is about to stop.
        //
        // A skip drops the partly pushed frame and plays its cue before the
        // new chapter. The audio already in the DMA buffer (up to
        // `BUFFER_BYTES`) still plays first, so a little of the old chapter
        // is heard; clearing it would mean restarting the transfer, which
        // clicks. A skip that cannot move plays nothing.
        //
        // After a skip, `pcm_crc` no longer matches a full decode on the host.
        match SKIP.swap(SKIP_NONE, Ordering::Relaxed) {
            SKIP_NONE => {}
            n if n > 0 => match decoder.next_chapter() {
                Ok(Skip::To(chapter)) => {
                    esp_println::println!("teddiebox: taf skipped to chapter {chapter}");
                    pending = 0..0;
                    instead = Some(Cue::SkipForward.samples());
                    end_after_cue = false;
                }
                // Skipping past the last chapter ends the story, on its cue.
                Ok(Skip::PastTheEnd) => {
                    esp_println::println!("teddiebox: taf skipped past the last chapter");
                    pending = 0..0;
                    instead = Some(Cue::SkipForward.samples());
                    end_after_cue = true;
                }
                Err(_) => esp_println::println!("teddiebox: taf could not skip forward"),
            },
            _ => match decoder.previous_chapter() {
                Ok(chapter) => {
                    esp_println::println!("teddiebox: taf skipped back to chapter {chapter}");
                    pending = 0..0;
                    instead = Some(Cue::SkipBack.samples());
                    end_after_cue = false;
                }
                Err(_) => esp_println::println!("teddiebox: taf could not skip back"),
            },
        }
        if let Some(cue) = take_cue() {
            over = Some(cue.samples());
        }
        // Checked before the buffer level, which cannot show this:
        // `available_bytes` counts descriptors the CPU owns, so it reads 0
        // both when the buffer is full and when the DMA has stopped.
        //
        // The DMA has stopped. `DmaTxStreamBuf` keeps a descriptor list that
        // ends in a null `next`; if the DMA reaches the end before the CPU
        // adds the next descriptor, it stops and does not restart by itself.
        // This can happen on any append, more often under load, even with the
        // buffer two-thirds full.
        //
        // So restart the transfer and carry on: a gap of a few milliseconds is
        // a glitch, but otherwise the story would go silent.
        if transfer.is_done() {
            restarts += 1;
            // Print when: restarts at the first frame point at the pre-fill;
            // restarts spread through playback are the descriptor race.
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
            if let Some(cue) = instead.as_mut() {
                if cue.fill(&mut scratch.pcm[..CUE_CHUNK]) {
                    instead = None;
                }
                pending = 0..CUE_CHUNK * 2;
            } else if end_after_cue {
                break;
            } else {
                let began = Instant::now();
                let decoded = decoder.next_frame(&mut scratch.pcm);
                decode_us += began.elapsed().as_micros();

                match decoded {
                    Ok(Some(samples)) => {
                        pcm_crc.update(as_bytes(&scratch.pcm[..samples]));
                        // After the checksum, which covers the story alone.
                        if let Some(cue) = over.as_mut() {
                            if cue.mix_into(&mut scratch.pcm[..samples]) {
                                over = None;
                            }
                        }
                        pending = 0..samples * 2;
                        frames += 1;
                        // Published for position memory, which cannot reach the
                        // decoder.
                        PAGE.store(decoder.page(), Ordering::Relaxed);
                    }
                    Ok(None) => break,
                    Err(_) => {
                        esp_println::println!("teddiebox: taf decode failed after {frames} frames");
                        break;
                    }
                }
            }
        }

        let pushed = transfer.push(&as_bytes(&scratch.pcm)[pending.clone()]);
        pending.start += pushed;
        if pushed == 0 {
            // The buffer is full and a frame is waiting. Let the DMA drain.
            Timer::after(BUFFER_FULL_WAIT).await;
            stalled += 1;
            // A full buffer for a second means nothing is draining it: a
            // stopped DMA also reads as full. Print the details once.
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
            // Print the checksum and decode cost so far, not only at the end,
            // since a whole Tonie is over half an hour. `taf2wav --frames N`
            // decodes the same number of frames on the host.
            esp_println::println!(
                "teddiebox: taf {} s, {frames} frames, buffer {}% (low {}%), {} underruns, crc32 {:08X}, decode {}%",
                so_far / 1_000_000,
                cushion.percent(),
                cushion.low_water_percent(),
                cushion.underruns(),
                pcm_crc.finish(),
                decode_us * 100 / so_far
            );
            // A stopped DMA reads as a *full* buffer (100%, no underruns).
            // These values tell the two apart.
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
        longest_gap_us = longest_gap_us.max(last_pass.elapsed().as_micros());
        last_pass = Instant::now();
    }

    // Do not wait for the buffer to drain after a stop: that would play
    // almost a second more.
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
        // DMA restarts are reported separately from underruns: the cushion
        // watches the buffer level and cannot see a stopped DMA, which reads
        // as full.
        "teddiebox: taf done — {} s, {frames} frames, low water {}%, {} underruns, {} dma restarts",
        elapsed.as_secs(),
        cushion.low_water_percent(),
        cushion.underruns(),
        restarts
    );
    esp_println::println!("teddiebox: taf pcm crc32 {:08X}", pcm_crc.finish());

    // Decode time as a percentage of the audio's length. Under 100 means the
    // box decodes faster than it plays.
    let played_us = elapsed.as_micros().max(1);
    let card_us = PAGE_READ_US.load(Ordering::Relaxed);
    esp_println::println!(
        "teddiebox: taf feeding the codec used {}% of real time — card reads {}%, decode {}%",
        decode_us * 100 / played_us,
        card_us * 100 / played_us,
        decode_us.saturating_sub(card_us) * 100 / played_us
    );
    let finish = if stopped {
        Finish::Stopped
    } else {
        Finish::Ended
    };
    Ok((finish, i2s_tx, buffer))
}
