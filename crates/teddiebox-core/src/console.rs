//! The bench console: commands the box accepts on UART0.
//!
//! Line matching only, so it can be tested on the host. What a command *does*
//! belongs to the firmware, which owns the hardware to do it with.

use heapless::String;

/// A command the box understands.
///
/// Not `Copy`: the credential commands carry a string, and a passphrase is not
/// something to be duplicated by accident.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Reboot into the ROM's UART download mode, for flashing.
    DownloadMode,
    /// Start the test tone.
    ///
    /// Opt-in rather than automatic: a tone that plays on every boot is
    /// unusable on a bench, and this box has no volume control of its own yet.
    Tone,
    /// Reboot into the application.
    ///
    /// Exists so a laptop can restart the box while watching its console.
    /// `esptool`'s reset takes exclusive hold of the serial port, so it cannot
    /// run while anything is capturing; writing two bytes can.
    Reboot,
    /// Mount the SD card and checksum what is on it.
    ///
    /// Opt-in for the same reason the tone is: it powers a rail, holds the bus
    /// for as long as the card is large, and has no business running on a boot
    /// that was not asking for it.
    Storage,
    /// Play the first WAV file on the card.
    ///
    /// Bench step 8, joining steps 6 and 7. Opt-in like the others: it powers
    /// a rail, takes the I2S peripheral for good, and is loud.
    PlayWav,
    /// Decode and play one named content file, `CONTENT/<dir>/<file>`.
    ///
    /// The Toniebox keeps its own system sounds under the reserved IDs
    /// `00000000` and `00000001`, a few tens of kilobytes each against tens of
    /// megabytes for a figure. Naming one is what makes them usable, both as a
    /// start-up sound and as a playback fixture that finishes in seconds.
    PlayContent { directory: u32, file: u32 },
    /// Play one of the box's own sounds, in whichever language it speaks.
    ///
    /// The same file ID means the same sound in all four language
    /// directories, so naming the directory as well is noise everywhere
    /// except when deliberately comparing languages.
    PlaySound { file: u32 },
    /// Stop whatever is playing.
    Stop,
    /// Decode the first TAF and print its samples, without playing them.
    ///
    /// Silent and as fast as the decoder goes, because this is a measurement
    /// rather than a listening test: what the host needs is the numbers.
    DumpPcm { frames: u8 },
    /// Decode and play the first TAF file on the card.
    ///
    /// Bench step 9. Opt-in like the rest, and the loudest thing here — it is
    /// real content rather than a test tone.
    PlayTaf,
    /// Bring the NFC reader up and report any tag on the plate.
    ///
    /// Bench step 10a. A plain ISO 15693 tag answers inventory; a Tonie in
    /// privacy mode does not, and looks identical to a wiring fault.
    Nfc,
    /// Remember the SLIX privacy password for this session.
    ///
    /// Typed at the bench rather than compiled in or kept on the card: it is a
    /// credential, and credentials have stayed out of this repository. It
    /// lives in RAM and dies with the next reset.
    Password(u32),
    /// Unlock a Tonie with the remembered password, then read its UID.
    ///
    /// Bench step 10b.
    Unlock,
    /// Send SET PASSWORD unconditionally, skipping the inventory that would
    /// otherwise answer first.
    ///
    /// A SLIX taken out of privacy mode stays out of it until something puts
    /// it back, and this driver has no command that does. So on the bench tag
    /// `Unlock` returns on its first inventory and the password exchange —
    /// the longest frame the reader sends, and the only one a tag can refuse
    /// — is never put on the air at all. This forces it.
    ForceUnlock,
    /// Put a tag back into privacy mode with the remembered password.
    ///
    /// The only command here that leaves a tag less readable than it found
    /// it. Stock firmware re-locks a figure after reading it, so this is what
    /// restores a bench tag to the state a figure actually arrives in — and
    /// without it the privacy path cannot be exercised twice.
    Lock,
    /// Dump a tag's memory, `count` blocks starting at `first`.
    ///
    /// The bench instrument behind the auth token. teddyCloud relays the box's
    /// `Authorization: BD <64 hex>` upstream without ever validating it, and
    /// revvox's protocol analysis calls that value the memory content of the
    /// tag — so it is readable with the reader already on this board.
    ///
    /// The range is typed at the bench rather than fixed here so that this
    /// command can contradict the guess it was written to test. It did the
    /// opposite on 2026-09-03: `mem 00 08` reads the token whole, and block 8
    /// onward does not answer, so the token is the entire user memory with
    /// nothing spare. The range stays typed because that is what found the
    /// boundary, and what would find a different one on a different tag.
    ReadMemory { first: u8, count: u8 },
    /// Ask the radio what access points it can hear.
    ///
    /// The first thing the radio is asked to do, and chosen because it needs
    /// no credentials, no network stack and no card: a scan that comes back
    /// empty is a statement about the radio or the antenna, never about a
    /// password. That separation is the whole reason it exists before
    /// anything that associates.
    NetScan,
    /// Remember the network name to associate with.
    ///
    /// Typed at the bench, held in RAM, gone at the next reset — the same
    /// treatment [`Command::Password`] gets, and for the same reason. This is
    /// a stop-gap: the design has credentials arriving from the card's
    /// `CONFIG.TXT`, which is why `net.rs` takes a whole `Config` rather than
    /// two strings.
    NetSsid(String<MAX_SSID>),
    /// Remember the passphrase to associate with.
    ///
    /// Never echoed, and the reason [`MAX_LINE`] is as long as it is.
    NetPassword(String<MAX_PASSPHRASE>),
    /// Associate with the remembered network and take a DHCP lease.
    NetUp,
    /// Open a TLS connection to the configured server and hang up.
    ///
    /// The smallest thing that exercises the transport on its own. Kept apart
    /// from fetching content deliberately: a failure here is the handshake,
    /// the cipher suite or the record layer, and a failure in a download is
    /// not — which is the difference between a bisect and a guess.
    NetTls,
    /// Report how deep the stack has ever gone.
    ///
    /// Sizing anything that shares DRAM with the stack — the audio buffer, a
    /// second core's stack — needs this number, and until now it has only ever
    /// been assumed.
    StackReport,
    /// Check the server's certificate, or don't.
    ///
    /// A bench override of the card's `insecure` key, the way
    /// [`Command::NetSsid`] overrides its `ssid`. It exists because the card
    /// lives inside the box and the bench does not, and because whether
    /// certificates are checked is the setting most worth being able to flip
    /// without a screwdriver.
    NetInsecure(bool),
    /// Download one content file and check it, without writing to the card.
    ///
    /// The eight bytes are the identifier **as it appears in the URL and in
    /// teddyCloud's listing** — the reversed UID — so that what is typed can
    /// be copied from the server and compared against it. `request.rs`
    /// reverses again on the way out, which is why the caller hands these
    /// over backwards.
    Get([u8; 8]),
    /// Play a file a download put in `/CACHE/`.
    ///
    /// Named by the same sixteen digits that fetched it, so `get X` and
    /// `play X` are the two halves of one job. The halves are already split
    /// here because that is how the directory and file are named on the card.
    PlayCache { directory: u32, file: u32 },
    /// Drop the association and power the modem down.
    NetDown,
    /// Report whether the radio is up, and on what address.
    NetStatus,
    /// Override one register of the codec's start-up sequence.
    ///
    /// Which register decides whether the box clicks on start-up can only be
    /// settled by ear, and a reflash between guesses makes that loop minutes
    /// long. Overrides are applied by `CodecInit`, not immediately, because
    /// the question is always what the *start-up* does.
    CodecSet { page: u8, register: u8, value: u8 },
    /// Forget every override, back to the compiled-in sequence.
    CodecClear,
    /// Take the codec down and bring it up again, so the start-up transient
    /// can be heard on demand rather than once per reboot.
    CodecInit,
    /// Run the codec's software power-down and nothing else.
    CodecDown,
    /// Power the codec's output path up or down.
    ///
    /// Powering the class-D amplifier and the DAC is what the box clicks on,
    /// so the start-up no longer does it and this is what makes the box
    /// audible at all. Eventually playback will ask for it; for now the bench
    /// asks, so the two can be heard apart.
    Output(bool),
    /// Mute or unmute the class-D speaker driver, now rather than at start-up.
    ///
    /// Unmuting is what the box clicks on, and the codec's soft-stepping only
    /// runs when it has a clock — which it does not have until audio is
    /// playing. Separating the two needs the mute reachable at any moment.
    Speaker(bool),
}

/// Longest network name accepted, in octets. 802.11 says 32.
pub const MAX_SSID: usize = 32;
/// Longest passphrase accepted. A WPA2 personal passphrase runs to 63
/// characters; the 64-character form is a raw PSK, which is a different thing
/// and not what is typed here.
pub const MAX_PASSPHRASE: usize = 63;

/// Longest command line accepted. Anything longer cannot be a command, and is
/// discarded rather than allowed to shift a buffer around.
///
/// Sized for `net pw <63 characters>`, which at seventy is far and away the
/// longest line this console takes — the next longest is `play <8 hex>/<8
/// hex>` at twenty-one. It grew from 24 for exactly that reason: a passphrase
/// silently truncated to fit would fail association with no hint why.
const MAX_LINE: usize = 72;

/// Watches a byte stream for a command line.
///
/// Deliberately line-oriented: the box prints a heartbeat forever and a
/// terminal may echo, so a command fires only on a complete line that matches
/// exactly. `ddl` is a typo, not a request to reboot.
#[derive(Debug)]
pub struct CommandWatch {
    line: [u8; MAX_LINE],
    len: usize,
    overflowed: bool,
}

impl Default for CommandWatch {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandWatch {
    pub const fn new() -> Self {
        Self {
            line: [0; MAX_LINE],
            len: 0,
            overflowed: false,
        }
    }

    /// Feeds one received byte. `Some` exactly once per matching line.
    pub fn feed(&mut self, byte: u8) -> Option<Command> {
        if byte == b'\r' || byte == b'\n' {
            let matched = if self.overflowed {
                None
            } else {
                match &self.line[..self.len] {
                    b"dl" => Some(Command::DownloadMode),
                    b"rb" => Some(Command::Reboot),
                    b"t" => Some(Command::Tone),
                    b"sd" => Some(Command::Storage),
                    b"wav" => Some(Command::PlayWav),
                    b"taf" => Some(Command::PlayTaf),
                    b"stop" => Some(Command::Stop),
                    b"nfc" => Some(Command::Nfc),
                    b"slix" => Some(Command::Unlock),
                    b"slixp" => Some(Command::ForceUnlock),
                    b"lock" => Some(Command::Lock),
                    b"cinit" => Some(Command::CodecInit),
                    b"cclr" => Some(Command::CodecClear),
                    b"cdown" => Some(Command::CodecDown),
                    b"out 1" => Some(Command::Output(true)),
                    b"out 0" => Some(Command::Output(false)),
                    b"spk 1" => Some(Command::Speaker(true)),
                    b"spk 0" => Some(Command::Speaker(false)),
                    b"net scan" => Some(Command::NetScan),
                    b"net up" => Some(Command::NetUp),
                    b"net tls" => Some(Command::NetTls),
                    b"stack" => Some(Command::StackReport),
                    b"net insecure yes" | b"net insecure true" => Some(Command::NetInsecure(true)),
                    b"net insecure no" | b"net insecure false" => Some(Command::NetInsecure(false)),
                    b"net down" => Some(Command::NetDown),
                    b"net status" => Some(Command::NetStatus),
                    other => parse_password(other)
                        .or_else(|| parse_codec_set(other))
                        .or_else(|| parse_play_content(other))
                        .or_else(|| parse_dump_pcm(other))
                        .or_else(|| parse_read_memory(other))
                        .or_else(|| parse_credential(other))
                        .or_else(|| parse_get(other)),
                }
            };
            self.len = 0;
            self.overflowed = false;
            return matched;
        }

        if self.len == MAX_LINE {
            self.overflowed = true;
        } else {
            self.line[self.len] = byte;
            self.len += 1;
        }
        None
    }
}

/// Reads exactly eight hex digits, as the Toniebox writes a content ID.
fn hex_u32(digits: &[u8]) -> Option<u32> {
    if digits.len() != 8 {
        return None;
    }
    let mut value = 0u32;
    for &byte in digits {
        let nibble = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => return None,
        };
        value = (value << 4) | u32::from(nibble);
    }
    Some(value)
}

/// Reads `pcm <2 hex>`, a frame count.
fn parse_dump_pcm(line: &[u8]) -> Option<Command> {
    let frames = hex_byte(line.strip_prefix(b"pcm ")?)?;
    Some(Command::DumpPcm { frames })
}

/// Reads `play <8 hex>/<8 hex>`, the path the box keeps its audio under.
fn parse_play_content(line: &[u8]) -> Option<Command> {
    let rest = line.strip_prefix(b"play ")?;
    // Sixteen digits is an identifier rather than a path: it is what `get`
    // takes, and what a download is filed under. Checked before the split so
    // that the two shorter forms keep their meaning exactly.
    if rest.len() == 16 {
        let directory = hex_u32(&rest[..8])?;
        let file = hex_u32(&rest[8..])?;
        return Some(Command::PlayCache { directory, file });
    }
    let mut halves = rest.split(|&b| b == b'/');
    let first = hex_u32(halves.next()?)?;
    // One half names a sound in the box's own language; two name a path, which
    // is what comparing languages needs.
    let Some(second) = halves.next() else {
        return Some(Command::PlaySound { file: first });
    };
    let file = hex_u32(second)?;
    if halves.next().is_some() {
        return None;
    }
    Some(Command::PlayContent {
        directory: first,
        file,
    })
}

/// Reads two hex digits, exactly.
///
/// Exactly two, because a register or a value written with one digit is a
/// typo rather than a small number, and these go to a live codec where a
/// wrong register is a write to something unrelated.
/// Parses `get <16 hex>` into the eight identifier bytes.
///
/// All or nothing: an identifier a digit short names a different figure, and
/// the server answers that with a `404` the box would report as "no content"
/// — a wrong answer wearing the shape of a right one.
fn parse_get(line: &[u8]) -> Option<Command> {
    let rest = line.strip_prefix(b"get ")?;
    if rest.len() != 16 {
        return None;
    }
    let mut uid = [0u8; 8];
    for (byte, digits) in uid.iter_mut().zip(rest.chunks_exact(2)) {
        *byte = hex_byte(digits)?;
    }
    Some(Command::Get(uid))
}

fn hex_byte(digits: &[u8]) -> Option<u8> {
    if digits.len() != 2 {
        return None;
    }
    let mut value = 0u8;
    for &byte in digits {
        let nibble = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => return None,
        };
        value = (value << 4) | nibble;
    }
    Some(value)
}

/// Reads `cset <page> <register> <value>`, the last two as two hex digits.
fn parse_codec_set(line: &[u8]) -> Option<Command> {
    let rest = line.strip_prefix(b"cset ")?;
    let mut parts = rest.split(|&b| b == b' ');
    // The codec has pages 0 and 1; nothing this firmware touches lives higher,
    // and a mistyped page would write to a quite different register.
    let page = match parts.next()? {
        b"0" => 0,
        b"1" => 1,
        _ => return None,
    };
    let register = hex_byte(parts.next()?)?;
    let value = hex_byte(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    Some(Command::CodecSet {
        page,
        register,
        value,
    })
}

/// Most blocks one `mem` command will read.
///
/// Bounds the reply buffer the firmware has to hold. Comfortably past the
/// eight blocks a SLIX-L is believed to carry, so the hypothesis can be
/// overshot and disproved rather than merely confirmed.
pub const MAX_MEMORY_BLOCKS: u8 = 32;

/// Reads `net ssid <name>` and `net pw <passphrase>`.
///
/// The value is taken verbatim to the end of the line: a passphrase may
/// contain spaces, and trimming it to be tidy would change the credential.
/// Empty is refused — it is a typo, and one the driver would otherwise be
/// handed as though it were meant. Too long is refused rather than truncated,
/// for the same reason.
fn parse_credential(line: &[u8]) -> Option<Command> {
    if let Some(rest) = line.strip_prefix(b"net ssid ") {
        let text = core::str::from_utf8(rest).ok()?;
        return String::try_from(text)
            .ok()
            .filter(|s| !s.is_empty())
            .map(Command::NetSsid);
    }
    let rest = line.strip_prefix(b"net pw ")?;
    let text = core::str::from_utf8(rest).ok()?;
    String::try_from(text)
        .ok()
        .filter(|s| !s.is_empty())
        .map(Command::NetPassword)
}

/// Reads `mem <2 hex first block> <2 hex block count>`.
///
/// Both are hex for the same reason the codec's are: block numbers are read
/// off a datasheet's tables, not counted out.
fn parse_read_memory(line: &[u8]) -> Option<Command> {
    let rest = line.strip_prefix(b"mem ")?;
    let mut parts = rest.split(|&b| b == b' ');
    let first = hex_byte(parts.next()?)?;
    let count = hex_byte(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    // Zero is a typo rather than a small request, and anything past the buffer
    // would have to be truncated — which would print a short dump that reads
    // exactly like a complete one.
    if count == 0 || count > MAX_MEMORY_BLOCKS {
        return None;
    }
    Some(Command::ReadMemory { first, count })
}

/// Reads `pw <8 hex digits>`.
///
/// Exactly eight, because a privacy password is a `u32` and a short one is a
/// typo rather than a small number. Getting it wrong matters more than usual:
/// a tag refuses a wrong password by staying silent, which is what an empty
/// plate and a broken antenna also look like.
fn parse_password(line: &[u8]) -> Option<Command> {
    let digits = line.strip_prefix(b"pw ")?;
    if digits.len() != 8 {
        return None;
    }

    let mut value: u32 = 0;
    for &byte in digits {
        let nibble = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => return None,
        };
        value = (value << 4) | u32::from(nibble);
    }
    Some(Command::Password(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(watch: &mut CommandWatch, bytes: &[u8]) -> Option<Command> {
        bytes.iter().find_map(|&b| watch.feed(b))
    }

    #[test]
    fn the_download_command_fires_on_its_terminator() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"dl"), None, "not until the line ends");
        assert_eq!(watch.feed(b'\r'), Some(Command::DownloadMode));
    }

    #[test]
    fn the_reboot_command_is_distinct() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"rb\n"), Some(Command::Reboot));
    }

    /// Line-oriented on purpose. An earlier matcher fired on `ddl` because it
    /// scanned for a substring; a mistyped line must not reboot the box.
    #[test]
    fn the_tone_command_is_a_single_letter() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"t\r"), Some(Command::Tone));
    }

    #[test]
    fn a_mistyped_line_does_not_fire() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"ddl\r"), None);
        assert_eq!(feed_all(&mut watch, b"dl \r"), None);
        assert_eq!(feed_all(&mut watch, b"DL\r"), None);
    }

    #[test]
    fn a_correct_line_after_a_wrong_one_still_fires() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"nonsense\rdl\r"),
            Some(Command::DownloadMode)
        );
    }

    #[test]
    fn the_storage_command_fires_on_its_own_line() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"sd\r"), Some(Command::Storage));
    }

    #[test]
    fn the_wav_command_fires_on_its_own_line() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"wav\r"), Some(Command::PlayWav));
    }

    #[test]
    fn the_taf_command_is_distinct_from_the_wav_one() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"taf\r"), Some(Command::PlayTaf));
        assert_eq!(feed_all(&mut watch, b"wav\r"), Some(Command::PlayWav));
    }

    #[test]
    fn the_nfc_command_fires_on_its_own_line() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"nfc\r"), Some(Command::Nfc));
    }

    #[test]
    fn the_unlock_command_is_distinct_from_the_nfc_one() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"slix\r"), Some(Command::Unlock));
    }

    /// The privacy path has to be reachable on a tag that does not need it.
    /// `slix` tries a plain inventory first and stops as soon as that answers,
    /// so on an unlocked tag SET PASSWORD is never sent at all — and the one
    /// exchange this bench most needs to provoke becomes unreachable.
    #[test]
    fn the_forced_unlock_command_is_distinct_from_the_unlock_one() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"slixp\r"), Some(Command::ForceUnlock));
        assert_eq!(feed_all(&mut watch, b"slix\r"), Some(Command::Unlock));
    }

    /// Locking is not the inverse of any command here — it is its own — and
    /// it must not be reachable by a typo of `slix` or `slixp`, because the
    /// tag it acts on stops answering afterwards.
    #[test]
    fn the_lock_command_is_distinct_from_the_unlock_ones() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"lock\r"), Some(Command::Lock));
        assert_eq!(feed_all(&mut watch, b"slix\r"), Some(Command::Unlock));
        assert_eq!(feed_all(&mut watch, b"slixp\r"), Some(Command::ForceUnlock));
    }

    /// The codec's pop behaviour is decided by a handful of register values,
    /// and which of them matters can only be settled by ear. Reflashing
    /// between each guess makes that loop minutes long, so the values are
    /// overridable from the console and the sequence is re-runnable.
    /// The class-D mute has to be reachable while the box is running, not
    /// only during start-up: whether unmuting clicks may depend on whether
    /// the codec has a clock, and it only has one once audio is playing.
    /// Every moment the box clicks — `rb`, `cinit` — contains the codec's
    /// software power-down, and every silent one does not. Running that step
    /// on its own, changing nothing else, is what tells the two apart.
    #[test]
    fn the_codec_power_down_command_fires_on_its_own_line() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"cdown\r"), Some(Command::CodecDown));
    }

    /// The box's own system sounds live at `CONTENT/00000000/` and
    /// `CONTENT/00000001/` alongside the figures, and `taf` can only reach
    /// whichever file it happens to find first. Naming one is what makes the
    /// short ones usable as fixtures — seconds instead of the half hour a
    /// real figure takes.
    #[test]
    fn the_play_command_carries_the_content_id_it_names() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"play 00000000/00000003\r"),
            Some(Command::PlayContent {
                directory: 0x0000_0000,
                file: 0x0000_0003
            })
        );
        assert_eq!(
            feed_all(&mut watch, b"play 1A2B3C4D/500304E0\r"),
            Some(Command::PlayContent {
                directory: 0x1A2B_3C4D,
                file: 0x5003_04E0
            })
        );
    }

    /// One half means "in this box's language", which is what almost every
    /// use wants: the same file ID is the same sound in all four directories.
    #[test]
    fn a_play_command_with_one_half_names_a_sound_not_a_path() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"play 00000000\r"),
            Some(Command::PlaySound { file: 0 })
        );
    }

    /// Both halves are eight hex digits: that is how the Toniebox names them
    /// on the card, and a short one would silently open a different file.
    #[test]
    fn a_malformed_content_id_does_not_fire() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"play 0000000/00000003\r"),
            None,
            "short"
        );
        assert_eq!(
            feed_all(&mut watch, b"play 0000000G/00000003\r"),
            None,
            "not hex"
        );
    }

    /// Stopping has to be its own word rather than a second `taf`: playback is
    /// loud, and the command that ends it must not be a typo away from one
    /// that starts it.
    /// Step 9's criterion is that the box's samples match the host's, and a
    /// checksum can only ever answer yes or no. Opus is not specified to be
    /// bit-exact across platforms, so the useful question is *how far apart*,
    /// and that needs the samples themselves.
    #[test]
    fn the_pcm_command_carries_how_many_frames_to_dump() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"pcm 53\r"),
            Some(Command::DumpPcm { frames: 0x53 })
        );
        assert_eq!(
            feed_all(&mut watch, b"pcm ff\r"),
            Some(Command::DumpPcm { frames: 0xFF })
        );
        assert_eq!(feed_all(&mut watch, b"pcm\r"), None, "no count");
        assert_eq!(
            feed_all(&mut watch, b"pcm 5\r"),
            None,
            "one digit is a typo"
        );
    }

    #[test]
    fn the_stop_command_fires_on_its_own_line() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"stop\r"), Some(Command::Stop));
    }

    #[test]
    fn the_output_command_carries_which_way_it_goes() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"out 1\r"),
            Some(Command::Output(true))
        );
        assert_eq!(
            feed_all(&mut watch, b"out 0\r"),
            Some(Command::Output(false))
        );
        assert_eq!(feed_all(&mut watch, b"out\r"), None, "no direction given");
    }

    #[test]
    fn the_speaker_command_carries_which_way_it_goes() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"spk 1\r"),
            Some(Command::Speaker(true))
        );
        assert_eq!(
            feed_all(&mut watch, b"spk 0\r"),
            Some(Command::Speaker(false))
        );
        assert_eq!(feed_all(&mut watch, b"spk\r"), None, "no direction given");
        assert_eq!(feed_all(&mut watch, b"spk 2\r"), None, "not a direction");
    }

    #[test]
    fn a_codec_override_carries_its_page_register_and_value() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"cset 1 21 be\r"),
            Some(Command::CodecSet {
                page: 1,
                register: 0x21,
                value: 0xBE
            })
        );
    }

    #[test]
    fn the_codec_rerun_and_clear_commands_fire_on_their_own_lines() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"cinit\r"), Some(Command::CodecInit));
        assert_eq!(feed_all(&mut watch, b"cclr\r"), Some(Command::CodecClear));
    }

    /// A mistyped override must not quietly become a different register. This
    /// writes to a live codec, where a wrong page is a write to something
    /// entirely unrelated.
    #[test]
    fn a_malformed_codec_override_does_not_fire() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"cset 1 21\r"), None, "no value");
        assert_eq!(
            feed_all(&mut watch, b"cset 1 2 be\r"),
            None,
            "short register"
        );
        assert_eq!(
            feed_all(&mut watch, b"cset 9 21 be\r"),
            None,
            "no such page"
        );
        assert_eq!(feed_all(&mut watch, b"cset 1 2g be\r"), None, "not hex");
    }

    #[test]
    fn the_password_command_carries_its_value() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"pw DEADBEEF\r"),
            Some(Command::Password(0xDEAD_BEEF))
        );
    }

    /// Lower case is what anyone actually types.
    #[test]
    fn a_password_in_lower_case_is_accepted() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"pw deadbeef\r"),
            Some(Command::Password(0xDEAD_BEEF))
        );
    }

    /// Leading zeroes are part of the value, not decoration: a password of
    /// 0x0000FFFF must not be read as 0xFFFF0000 or refused.
    #[test]
    fn a_password_with_leading_zeroes_keeps_its_width() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"pw 0000ffff\r"),
            Some(Command::Password(0x0000_FFFF))
        );
    }

    /// A mistyped password must not silently become a different one. Sending
    /// the wrong value to a tag is indistinguishable from an empty plate, so a
    /// typo would look like a hardware fault.
    #[test]
    fn a_malformed_password_does_not_fire() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"pw DEADBEE\r"), None, "too short");
        assert_eq!(feed_all(&mut watch, b"pw DEADBEEFF\r"), None, "too long");
        assert_eq!(feed_all(&mut watch, b"pw DEADBEEG\r"), None, "not hex");
        assert_eq!(feed_all(&mut watch, b"pw\r"), None, "no value at all");
    }

    /// A heartbeat prints once a second forever. None of it may look like a
    /// command, including any prefix of it.
    #[test]
    fn ordinary_traffic_does_not_fire_anything() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"teddiebox: alive 41\r\n"), None);
        assert_eq!(
            feed_all(&mut watch, b"teddiebox: accel -6912 1216 14656\r\n"),
            None
        );
    }

    /// An overlong line is discarded whole rather than having its tail matched.
    #[test]
    fn an_overlong_line_cannot_match_by_its_ending() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"aaaaaaaaaaaaadl\r"), None);
    }

    /// The instrument that answers "which blocks hold the token". The range is
    /// typed rather than fixed, because pinning it to the eight-block guess
    /// would stop it from ever disagreeing with that guess.
    #[test]
    fn a_memory_dump_names_its_first_block_and_a_count() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"mem 00 08\r"),
            Some(Command::ReadMemory { first: 0, count: 8 })
        );
    }

    /// Reading past the end is the point: where the tag stops answering is the
    /// measurement, so a first block beyond the guessed user memory is legal.
    #[test]
    fn a_memory_dump_may_start_past_the_expected_end() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"mem 1f 01\r"),
            Some(Command::ReadMemory {
                first: 0x1F,
                count: 1
            })
        );
    }

    /// The reply buffer is fixed, so a count it cannot hold is refused rather
    /// than quietly truncated into a dump that reads as complete.
    #[test]
    fn a_memory_dump_longer_than_the_buffer_is_refused() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"mem 00 21\r"), None);
    }

    /// Nothing to read is a typo, not a request.
    #[test]
    fn a_memory_dump_of_no_blocks_is_refused() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"mem 00 00\r"), None);
    }

    /// The first radio command, and deliberately the one that needs no
    /// credentials: a scan that finds nothing is a statement about the radio,
    /// not about a password.
    #[test]
    fn the_scan_command_asks_the_radio_what_it_can_hear() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"net scan\r"), Some(Command::NetScan));
    }

    /// `net` alone does nothing yet, and must not be mistaken for a command
    /// that does.
    #[test]
    fn net_without_a_verb_does_not_fire() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"net\r"), None);
    }

    /// Credentials are typed at the bench for now, so the console has to carry
    /// a string rather than a number for the first time.
    #[test]
    fn the_ssid_command_carries_the_name_it_names() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"net ssid example-ssid\r"),
            Some(Command::NetSsid(
                heapless::String::try_from("example-ssid").unwrap()
            ))
        );
    }

    /// An SSID is 32 octets at most (802.11), and one octet more is a typo
    /// rather than a name to be quietly cut down to fit.
    #[test]
    fn an_ssid_longer_than_the_standard_allows_is_refused() {
        let mut watch = CommandWatch::new();
        let mut line = heapless::Vec::<u8, 80>::new();
        line.extend_from_slice(b"net ssid ").unwrap();
        line.extend_from_slice(&[b'a'; 33]).unwrap();
        line.push(b'\r').unwrap();
        assert_eq!(feed_all(&mut watch, &line), None);
    }

    /// A WPA2 passphrase runs to 63 characters, which is longer than every
    /// other line this console has ever accepted.
    #[test]
    fn a_passphrase_of_the_full_length_still_fits_a_line() {
        let mut watch = CommandWatch::new();
        let mut line = heapless::Vec::<u8, 80>::new();
        line.extend_from_slice(b"net pw ").unwrap();
        line.extend_from_slice(&[b'x'; 63]).unwrap();
        line.push(b'\r').unwrap();
        let raw = [b'x'; 63];
        let expected = core::str::from_utf8(&raw).unwrap();
        assert_eq!(
            feed_all(&mut watch, &line),
            Some(Command::NetPassword(
                heapless::String::try_from(expected).unwrap()
            ))
        );
    }

    /// An empty credential is a typo, and one that would otherwise be handed
    /// to the driver as if it were meant.
    #[test]
    fn an_empty_ssid_is_refused() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"net ssid \r"), None);
    }

    #[test]
    fn the_radio_can_be_asked_up_down_and_for_its_state() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"net up\r"), Some(Command::NetUp));
        assert_eq!(feed_all(&mut watch, b"net down\r"), Some(Command::NetDown));
        assert_eq!(
            feed_all(&mut watch, b"net status\r"),
            Some(Command::NetStatus)
        );
    }
    /// `net tls` is the smallest thing that exercises the transport: it
    /// connects, handshakes, asks one question and hangs up. Separate from
    /// `get` on purpose — a failure here is the transport, and a failure there
    /// is not.
    /// The card is the place this belongs, but the card is inside the box and
    /// the bench is not. Same relationship `net ssid` has to the card's ssid.
    #[test]
    fn net_insecure_takes_the_same_words_the_config_file_takes() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"net insecure yes\n"),
            Some(Command::NetInsecure(true))
        );
        assert_eq!(
            feed_all(&mut watch, b"net insecure no\n"),
            Some(Command::NetInsecure(false))
        );
        assert_eq!(
            feed_all(&mut watch, b"net insecure true\n"),
            Some(Command::NetInsecure(true))
        );
    }

    /// A word nobody recognises must not resolve to "off" quietly, for the
    /// same reason the config file refuses one: this line decides whether
    /// certificates are checked.
    #[test]
    fn an_unrecognised_insecure_word_is_not_a_command() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"net insecure maybe\n"), None);
    }

    #[test]
    fn net_tls_is_recognised() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"net tls\n"), Some(Command::NetTls));
    }
    /// `get` takes the identifier **as it appears in the URL and in
    /// teddyCloud's own listing** — the reversed UID — because that is the
    /// string a person can copy from the server and compare against. The
    /// figure on the bench is UID `E0040350503F2E1D`, which teddyCloud files
    /// under `1D2E3F50500304E0`.
    #[test]
    fn get_takes_the_reversed_uid_as_it_appears_on_the_server() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"get 1D2E3F50500304E0\n"),
            Some(Command::Get([
                0x1D, 0x2E, 0x3F, 0x50, 0x50, 0x03, 0x04, 0xE0
            ]))
        );
    }

    #[test]
    fn get_accepts_lower_case_hex() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"get 1a2b3c4d500304e0\n"),
            Some(Command::Get([
                0x1A, 0x2B, 0x3C, 0x4D, 0x50, 0x03, 0x04, 0xE0
            ]))
        );
    }

    /// Short, long, or not hex is not a command. An identifier one digit out
    /// names a different figure, and the server answers that with a 404 the
    /// box would report as "no content" — a wrong answer that looks like a
    /// right one.
    #[test]
    fn a_malformed_identifier_is_not_a_command() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"get 1D2E3F50500304E\n"), None);
        assert_eq!(feed_all(&mut watch, b"get 1D2E3F50500304E00\n"), None);
        assert_eq!(feed_all(&mut watch, b"get 1D2E3F50500304EZ\n"), None);
        assert_eq!(feed_all(&mut watch, b"get \n"), None);
    }
    /// The measurement three experiments this session guessed at instead.
    #[test]
    fn stack_is_recognised() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"stack\n"), Some(Command::StackReport));
    }
    /// The other half of `get`. A download lands in `/CACHE/` under the same
    /// identifier that fetched it, so the same sixteen digits play it back —
    /// which is what closes the loop from "the box fetched a story" to "the box
    /// tells it".
    #[test]
    fn play_takes_a_ruid_to_mean_the_cache() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"play 1A2B3C4D500304E0\n"),
            Some(Command::PlayCache {
                directory: 0x1A2B_3C4D,
                file: 0x5003_04E0
            })
        );
    }

    /// And the two older forms keep their meaning: eight digits is a sound in
    /// the box's own language, eight and eight is a path under `CONTENT`.
    #[test]
    fn the_shorter_play_forms_still_mean_what_they_did() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"play 00000010\n"),
            Some(Command::PlaySound { file: 0x10 })
        );
        assert_eq!(
            feed_all(&mut watch, b"play 00000001/00000000\n"),
            Some(Command::PlayContent {
                directory: 1,
                file: 0
            })
        );
    }
}
