//! The box's short sounds for the ears and skips: the cues.
//!
//! Generated as they play rather than read from the card. The notes, lengths
//! and envelope were measured from recordings of a stock box and confirmed by
//! ear against them.
//!
//! `no_std` has no `sin`, `cos` or `exp`, so each tone is a resonator (two
//! multiplies per sample) started from short series for the few constants it
//! needs.

/// Frames per second.
pub const SAMPLE_RATE_HZ: u32 = 48_000;

/// The loudest a cue gets: half of full scale. Judged by ear on the box over
/// a playing story, at every volume step: clearly heard, never startling.
pub const CUE_PEAK: i16 = 16_384;

/// A sound the box makes about a press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cue {
    /// D5 then G5: the story moves on a chapter.
    SkipForward,
    /// D5 then A4: the story moves back a chapter.
    SkipBack,
    VolumeUp,
    VolumeDown,
    /// Two short beeps: the volume is already at its top or bottom.
    VolumeLimit,
}

#[derive(Debug, Clone, Copy)]
enum Segment {
    /// A plain sine.
    Beep {
        hz: f32,
        frames: u32,
    },
    /// A sine with the skip notes' overtones and fade.
    Note {
        hz: f32,
        frames: u32,
    },
    Silence {
        frames: u32,
    },
}

impl Segment {
    const fn frames(self) -> u32 {
        match self {
            Segment::Beep { frames, .. } | Segment::Note { frames, .. } => frames,
            Segment::Silence { frames } => frames,
        }
    }
}

/// 92.6 ms, 61.4 ms and 84.8 ms: the skip's first note, gap and second note.
const SKIP_FIRST: u32 = 4_445;
const SKIP_GAP: u32 = 2_947;
const SKIP_SECOND: u32 = 4_070;
/// 20 ms, the length of every volume beep.
const BEEP: u32 = 960;
/// 30 ms between the two limit beeps.
const LIMIT_GAP: u32 = 1_440;

static SKIP_FORWARD: [Segment; 3] = [
    Segment::Note {
        hz: 587.33,
        frames: SKIP_FIRST,
    },
    Segment::Silence { frames: SKIP_GAP },
    Segment::Note {
        hz: 783.99,
        frames: SKIP_SECOND,
    },
];
static SKIP_BACK: [Segment; 3] = [
    Segment::Note {
        hz: 587.33,
        frames: SKIP_FIRST,
    },
    Segment::Silence { frames: SKIP_GAP },
    Segment::Note {
        hz: 440.0,
        frames: SKIP_SECOND,
    },
];
static VOLUME_UP: [Segment; 1] = [Segment::Beep {
    hz: 800.0,
    frames: BEEP,
}];
static VOLUME_DOWN: [Segment; 1] = [Segment::Beep {
    hz: 504.0,
    frames: BEEP,
}];
static VOLUME_LIMIT: [Segment; 3] = [
    Segment::Beep {
        hz: 1_000.0,
        frames: BEEP,
    },
    Segment::Silence { frames: LIMIT_GAP },
    Segment::Beep {
        hz: 1_000.0,
        frames: BEEP,
    },
];

/// The skip notes' 2nd and 3rd harmonics: −26 dB and −42 dB.
const SECOND_HARMONIC: f32 = 0.050_12;
const THIRD_HARMONIC: f32 = 0.007_94;
/// The most a note's three components can add up to, so a note never
/// exceeds [`CUE_PEAK`].
const NOTE_HEADROOM: f32 = 1.0 + SECOND_HARMONIC + THIRD_HARMONIC;
/// Per-frame factor of the skip notes' fade: e^(−1/(75 ms × 48 kHz)).
const FADE_PER_FRAME: f32 = 0.999_722_3;
/// Frames in each smooth edge: 1 ms.
const EDGE: u32 = 48;

impl Cue {
    fn segments(self) -> &'static [Segment] {
        match self {
            Cue::SkipForward => &SKIP_FORWARD,
            Cue::SkipBack => &SKIP_BACK,
            Cue::VolumeUp => &VOLUME_UP,
            Cue::VolumeDown => &VOLUME_DOWN,
            Cue::VolumeLimit => &VOLUME_LIMIT,
        }
    }

    /// How long the cue lasts, in frames.
    pub fn frames(self) -> u32 {
        self.segments().iter().map(|s| s.frames()).sum()
    }

    /// The cue's samples, from its first frame.
    pub fn samples(self) -> CueSamples {
        let segments = self.segments();
        let mut samples = CueSamples {
            segments,
            index: 0,
            frame: 0,
            tones: [Resonator::SILENT; 3],
            fade: 1.0,
        };
        samples.enter(segments[0]);
        samples
    }

    /// The byte that carries this cue between tasks. 0 is left for "none".
    pub const fn code(self) -> u8 {
        match self {
            Cue::SkipForward => 1,
            Cue::SkipBack => 2,
            Cue::VolumeUp => 3,
            Cue::VolumeDown => 4,
            Cue::VolumeLimit => 5,
        }
    }

    /// The cue a byte names, or `None` if it names none.
    pub const fn from_code(code: u8) -> Option<Cue> {
        match code {
            1 => Some(Cue::SkipForward),
            2 => Some(Cue::SkipBack),
            3 => Some(Cue::VolumeUp),
            4 => Some(Cue::VolumeDown),
            5 => Some(Cue::VolumeLimit),
            _ => None,
        }
    }
}

/// A cue being played: where it has got to, and its oscillators' state.
#[derive(Debug, Clone)]
pub struct CueSamples {
    segments: &'static [Segment],
    index: usize,
    frame: u32,
    /// The fundamental and, for notes, the 2nd and 3rd harmonics.
    tones: [Resonator; 3],
    fade: f32,
}

impl CueSamples {
    /// Writes the next samples into interleaved stereo `out`, and silence
    /// once the cue is over. Returns whether the cue has finished.
    pub fn fill(&mut self, out: &mut [i16]) -> bool {
        debug_assert!(out.len().is_multiple_of(2), "stereo takes pairs");
        for frame in out.chunks_exact_mut(2) {
            let sample = self.next_frame().unwrap_or(0);
            frame[0] = sample;
            frame[1] = sample;
        }
        self.is_finished()
    }

    /// Adds the next samples onto interleaved stereo `out`, clipping at full
    /// scale, and leaves the rest of `out` alone once the cue is over.
    /// Returns whether the cue has finished.
    pub fn mix_into(&mut self, out: &mut [i16]) -> bool {
        debug_assert!(out.len().is_multiple_of(2), "stereo takes pairs");
        for frame in out.chunks_exact_mut(2) {
            let Some(sample) = self.next_frame() else {
                break;
            };
            frame[0] = frame[0].saturating_add(sample);
            frame[1] = frame[1].saturating_add(sample);
        }
        self.is_finished()
    }

    fn is_finished(&self) -> bool {
        match self.segments.get(self.index) {
            None => true,
            Some(last) => self.index + 1 == self.segments.len() && self.frame >= last.frames(),
        }
    }

    /// Starts the oscillators for a segment.
    fn enter(&mut self, segment: Segment) {
        self.frame = 0;
        self.fade = 1.0;
        self.tones = match segment {
            Segment::Beep { hz, .. } | Segment::Note { hz, .. } => {
                let step = 2.0 * core::f32::consts::PI * hz / SAMPLE_RATE_HZ as f32;
                [
                    Resonator::new(step),
                    Resonator::new(2.0 * step),
                    Resonator::new(3.0 * step),
                ]
            }
            Segment::Silence { .. } => [Resonator::SILENT; 3],
        };
    }

    fn next_frame(&mut self) -> Option<i16> {
        let mut segment = *self.segments.get(self.index)?;
        while self.frame >= segment.frames() {
            self.index += 1;
            segment = *self.segments.get(self.index)?;
            self.enter(segment);
        }
        let n = self.frame;
        self.frame += 1;
        let level = match segment {
            Segment::Silence { .. } => 0.0,
            Segment::Beep { frames, .. } => self.tones[0].next() * edge(n, frames),
            Segment::Note { frames, .. } => {
                let wave = self.tones[0].next()
                    + SECOND_HARMONIC * self.tones[1].next()
                    + THIRD_HARMONIC * self.tones[2].next();
                let envelope = 0.5 + 0.5 * self.fade;
                self.fade *= FADE_PER_FRAME;
                wave / NOTE_HEADROOM * envelope * edge(n, frames)
            }
        };
        Some((level * f32::from(CUE_PEAK)) as i16)
    }
}

/// Gain for frame `n` of a tone `frames` long: rising over the first
/// [`EDGE`] frames, falling over the last, 1 between. Exactly 0 on the first
/// and last frame, so no tone starts or stops with a jump.
fn edge(n: u32, frames: u32) -> f32 {
    let from_end = frames - 1 - n;
    let near = n.min(from_end);
    if near >= EDGE {
        return 1.0;
    }
    // Smoothstep: the same shape as a raised cosine, without `cos`.
    let t = near as f32 / EDGE as f32;
    t * t * (3.0 - 2.0 * t)
}

/// A sine oscillator: sin(n·step), from sin(n+1) = 2cos(step)·sin(n) − sin(n−1).
#[derive(Debug, Clone, Copy)]
struct Resonator {
    twice_cos: f32,
    now: f32,
    before: f32,
}

impl Resonator {
    const SILENT: Resonator = Resonator {
        twice_cos: 0.0,
        now: 0.0,
        before: 0.0,
    };

    fn new(step: f32) -> Self {
        Resonator {
            twice_cos: 2.0 * cos(step),
            now: 0.0,
            before: -sin(step),
        }
    }

    /// This frame's value, then steps to the next.
    fn next(&mut self) -> f32 {
        let value = self.now;
        let next = self.twice_cos * self.now - self.before;
        self.before = self.now;
        self.now = next;
        value
    }
}

/// Series good to f32 precision for |x| ≤ 0.32, which covers the 3rd
/// harmonic of the highest note (2352 Hz at 48 kHz).
fn sin(x: f32) -> f32 {
    let x2 = x * x;
    x * (1.0 - x2 / 6.0 * (1.0 - x2 / 20.0 * (1.0 - x2 / 42.0)))
}

fn cos(x: f32) -> f32 {
    let x2 = x * x;
    1.0 - x2 / 2.0 * (1.0 - x2 / 12.0 * (1.0 - x2 / 30.0 * (1.0 - x2 / 56.0)))
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec;
    use std::vec::Vec;

    use super::*;

    const ALL: [Cue; 5] = [
        Cue::SkipForward,
        Cue::SkipBack,
        Cue::VolumeUp,
        Cue::VolumeDown,
        Cue::VolumeLimit,
    ];

    /// The whole cue, as the left channel.
    fn render(cue: Cue) -> Vec<i16> {
        let mut out = vec![0i16; cue.frames() as usize * 2];
        cue.samples().fill(&mut out);
        out.iter().step_by(2).copied().collect()
    }

    /// The pitch of a tone, from its rising zero crossings.
    fn pitch(tone: &[i16]) -> f64 {
        let rising: Vec<usize> = (1..tone.len())
            .filter(|&i| tone[i - 1] <= 0 && tone[i] > 0)
            .collect();
        let first = rising[0];
        let last = rising[rising.len() - 1];
        (rising.len() - 1) as f64 * 48_000.0 / (last - first) as f64
    }

    fn assert_pitch(tone: &[i16], hz: f64) {
        let measured = pitch(tone);
        assert!(
            (measured - hz).abs() <= hz * 0.005,
            "expected {hz} Hz, measured {measured:.2} Hz"
        );
    }

    /// Amplitude of the component at `hz`, by correlation.
    fn amplitude(tone: &[i16], hz: f64) -> f64 {
        let (mut s, mut c) = (0.0, 0.0);
        for (n, &x) in tone.iter().enumerate() {
            let phase = 2.0 * core::f64::consts::PI * hz * n as f64 / 48_000.0;
            s += f64::from(x) * phase.sin();
            c += f64::from(x) * phase.cos();
        }
        (s * s + c * c).sqrt()
    }

    /// A skip's first note, the gap and its second note: 92.6 ms, 61.4 ms and
    /// 84.8 ms at 48 kHz, as recorded from a stock box.
    fn skip_parts(s: &[i16]) -> (&[i16], &[i16], &[i16]) {
        (&s[..4_445], &s[4_445..4_445 + 2_947], &s[4_445 + 2_947..])
    }

    /// The limit cue's two 20 ms beeps and the 30 ms between them.
    fn limit_parts(s: &[i16]) -> (&[i16], &[i16], &[i16]) {
        (&s[..960], &s[960..960 + 1_440], &s[960 + 1_440..])
    }

    fn peak(samples: &[i16]) -> i16 {
        samples.iter().map(|s| s.saturating_abs()).max().unwrap()
    }

    /// Lengths measured from recordings of a stock box, in frames at 48 kHz.
    #[test]
    fn each_cue_lasts_as_long_as_the_recording() {
        // Given
        let cues = ALL;

        // When
        let lengths = cues.map(|cue| (cue, cue.frames()));

        // Then
        assert_eq!(
            lengths,
            [
                (Cue::SkipForward, 11_462),
                (Cue::SkipBack, 11_462),
                (Cue::VolumeUp, 960),
                (Cue::VolumeDown, 960),
                (Cue::VolumeLimit, 3_360),
            ]
        );
    }

    #[test]
    fn skip_forward_rises_from_d5_to_g5() {
        // Given
        let cue = Cue::SkipForward;

        // When
        let s = render(cue);

        // Then
        let (first, _, second) = skip_parts(&s);
        assert_pitch(first, 587.33);
        assert_pitch(second, 783.99);
    }

    #[test]
    fn skip_back_falls_from_d5_to_a4() {
        // Given
        let cue = Cue::SkipBack;

        // When
        let s = render(cue);

        // Then
        let (first, _, second) = skip_parts(&s);
        assert_pitch(first, 587.33);
        assert_pitch(second, 440.0);
    }

    #[test]
    fn the_volume_beeps_have_their_measured_pitches() {
        // Given
        let (up, down, limit) = (Cue::VolumeUp, Cue::VolumeDown, Cue::VolumeLimit);

        // When
        let (up, down, limit) = (render(up), render(down), render(limit));

        // Then
        assert_pitch(&up, 800.0);
        assert_pitch(&down, 504.0);
        let (first, _, second) = limit_parts(&limit);
        assert_pitch(first, 1_000.0);
        assert_pitch(second, 1_000.0);
    }

    #[test]
    fn the_gap_between_a_skips_notes_is_silent() {
        // Given
        let cue = Cue::SkipForward;

        // When
        let skip = render(cue);

        // Then
        let (_, gap, _) = skip_parts(&skip);
        assert!(gap.iter().all(|&s| s == 0));
    }

    #[test]
    fn the_gap_between_the_limit_beeps_is_silent() {
        // Given
        let cue = Cue::VolumeLimit;

        // When
        let limit = render(cue);

        // Then
        let (_, gap, _) = limit_parts(&limit);
        assert!(gap.iter().all(|&s| s == 0));
    }

    /// A jump from or to silence is a click.
    #[test]
    fn every_cue_starts_and_ends_at_zero() {
        // Given
        let cues = ALL;

        // When
        let ends = cues.map(|cue| {
            let s = render(cue);
            (cue, s[0], s[s.len() - 1])
        });

        // Then
        assert_eq!(ends, cues.map(|cue| (cue, 0, 0)));
    }

    #[test]
    fn no_cue_is_louder_than_the_peak() {
        // Given
        let cues = ALL;

        // When
        let peaks = cues.map(|cue| (cue, peak(&render(cue))));

        // Then
        for (cue, peak) in peaks {
            assert!(peak <= 16_384, "{cue:?} peaks at {peak}");
        }
    }

    /// Guards against a cue that is correct in pitch but far too quiet: 95 %
    /// of the 16,384 peak.
    #[test]
    fn the_beeps_reach_the_peak() {
        // Given
        let beep = Cue::VolumeUp;

        // When
        let loudest = peak(&render(beep));

        // Then
        assert!(loudest >= 15_564, "peaks at {loudest}");
    }

    /// A skip note starts at full level and has fallen to about two thirds
    /// by its end: 0.5 + 0.5·e^(−t/75 ms) at t ≈ 88 ms.
    #[test]
    fn a_skip_note_fades_as_it_plays() {
        // Given
        let skip = render(Cue::SkipForward);
        let (note, _, _) = skip_parts(&skip);

        // When: the peaks just inside its rising and falling edges
        let start = f64::from(peak(&note[48..480]));
        let end = f64::from(peak(&note[note.len() - 528..note.len() - 48]));

        // Then
        let ratio = end / start;
        assert!((0.62..=0.71).contains(&ratio), "ratio {ratio:.3}");
    }

    /// Measured −26 dB on the stock box's skip note.
    #[test]
    fn a_skip_note_carries_its_second_harmonic_26_db_down() {
        // Given
        let skip = render(Cue::SkipForward);
        let (note, _, _) = skip_parts(&skip);

        // When
        let db = 20.0 * (amplitude(note, 2.0 * 587.33) / amplitude(note, 587.33)).log10();

        // Then
        assert!((-27.0..=-25.0).contains(&db), "{db:.1} dB");
    }

    /// Measured −42 dB on the stock box's skip note.
    #[test]
    fn a_skip_note_carries_its_third_harmonic_42_db_down() {
        // Given
        let skip = render(Cue::SkipForward);
        let (note, _, _) = skip_parts(&skip);

        // When
        let db = 20.0 * (amplitude(note, 3.0 * 587.33) / amplitude(note, 587.33)).log10();

        // Then
        assert!((-43.0..=-41.0).contains(&db), "{db:.1} dB");
    }

    #[test]
    fn both_channels_carry_the_same_sample() {
        // Given
        let mut out = vec![0i16; 960 * 2];

        // When
        Cue::VolumeUp.samples().fill(&mut out);

        // Then
        assert!(out.chunks_exact(2).all(|frame| frame[0] == frame[1]));
    }

    /// The DMA takes what fits, so the firmware fills in whatever pieces it
    /// has room for.
    #[test]
    fn filling_in_pieces_gives_the_same_samples() {
        // Given
        let cue = Cue::SkipBack;
        let mut whole = vec![0i16; cue.frames() as usize * 2];
        cue.samples().fill(&mut whole);

        // When
        let mut pieces = Vec::new();
        let mut samples = cue.samples();
        for size in [2usize, 14, 1_000, 6].iter().cycle() {
            let mut piece = vec![0i16; *size];
            samples.fill(&mut piece);
            pieces.extend_from_slice(&piece);
            if pieces.len() >= whole.len() {
                break;
            }
        }

        // Then
        assert_eq!(&pieces[..whole.len()], &whole[..]);
    }

    #[test]
    fn fill_reports_the_end_only_once_the_last_frame_is_out() {
        // Given
        let mut samples = Cue::VolumeUp.samples();

        // When
        let after_all_but_one = samples.fill(&mut vec![0i16; 959 * 2]);
        let after_the_last = samples.fill(&mut [0i16; 2]);

        // Then
        assert_eq!((after_all_but_one, after_the_last), (false, true));
    }

    #[test]
    fn a_finished_cue_fills_with_silence() {
        // Given
        let mut samples = Cue::VolumeUp.samples();
        samples.fill(&mut vec![0i16; 960 * 2]);

        // When
        let mut after = [7i16; 8];
        let finished = samples.fill(&mut after);

        // Then
        assert!(finished);
        assert_eq!(after, [0; 8]);
    }

    #[test]
    fn a_fresh_cursor_starts_the_cue_again() {
        // Given: a cursor that has played part of the cue
        let mut half_used = Cue::VolumeDown.samples();
        half_used.fill(&mut vec![0i16; 400 * 2]);

        // When
        let mut first = vec![0i16; 100 * 2];
        Cue::VolumeDown.samples().fill(&mut first);

        // Then
        let mut from_the_start = vec![0i16; 100 * 2];
        Cue::VolumeDown.samples().fill(&mut from_the_start);
        assert_eq!(first, from_the_start);
    }

    #[test]
    fn mixing_onto_silence_is_filling() {
        // Given
        let mut filled = vec![0i16; 3_360 * 2];
        Cue::VolumeLimit.samples().fill(&mut filled);

        // When
        let mut mixed = vec![0i16; 3_360 * 2];
        Cue::VolumeLimit.samples().mix_into(&mut mixed);

        // Then
        assert_eq!(filled, mixed);
    }

    /// Wrapping would turn the loudest story sample into its opposite: a
    /// crack instead of a clipped beep.
    #[test]
    fn mixing_clips_instead_of_wrapping() {
        // Given
        let mut loud = vec![30_000i16; 960 * 2];

        // When
        Cue::VolumeUp.samples().mix_into(&mut loud);

        // Then
        assert!(loud.iter().all(|&s| s > 0), "a sample wrapped negative");
        assert!(loud.contains(&i16::MAX));
    }

    #[test]
    fn mixing_clips_at_the_negative_rail_too() {
        // Given
        let mut loud = vec![-30_000i16; 960 * 2];

        // When
        Cue::VolumeUp.samples().mix_into(&mut loud);

        // Then
        assert!(loud.iter().all(|&s| s < 0), "a sample wrapped positive");
        assert!(loud.contains(&i16::MIN));
    }

    /// Mixing past the end of the cue leaves the story untouched.
    #[test]
    fn mixing_stops_at_the_end_of_the_cue() {
        // Given
        let mut story = vec![123i16; 1_000 * 2];

        // When
        let finished = Cue::VolumeUp.samples().mix_into(&mut story);

        // Then
        assert!(finished);
        assert!(story[960 * 2..].iter().all(|&s| s == 123));
    }

    /// The byte that carries a cue between tasks. Literal, so the test can
    /// disagree with the table.
    #[test]
    fn each_cue_has_its_own_code() {
        // Given
        let cues = ALL;

        // When
        let codes = cues.map(|cue| (cue, cue.code()));

        // Then
        assert_eq!(
            codes,
            [
                (Cue::SkipForward, 1),
                (Cue::SkipBack, 2),
                (Cue::VolumeUp, 3),
                (Cue::VolumeDown, 4),
                (Cue::VolumeLimit, 5),
            ]
        );
    }

    #[test]
    fn every_cue_survives_the_trip_through_its_code() {
        // Given
        let cues = ALL;

        // When
        let round_tripped = cues.map(|cue| Cue::from_code(cue.code()));

        // Then
        assert_eq!(round_tripped, cues.map(Some));
    }

    /// 0 means no cue.
    #[test]
    fn a_byte_that_names_no_cue_is_refused() {
        // Given
        let strays = [0, 6];

        // When
        let cues = strays.map(Cue::from_code);

        // Then
        assert_eq!(cues, [None, None]);
    }
}
