#![no_std]

//! The serial console: commands the box accepts on UART0.
//!
//! Only parses lines, so it can be tested on the host. The firmware carries
//! out the commands.
//!
//! A separate crate from `teddiebox-core`, so changes to the console cannot
//! affect the reducer. `hex` lives here because the only users of a hex
//! password are a console command and the build that compiles one in.

pub mod hex;

use heapless::String;

/// A command the box understands.
///
/// Not `Copy`: some commands carry a passphrase, which should not be copied
/// by accident.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Reboot into the ROM's UART download mode, for flashing.
    DownloadMode,
    /// Start the test tone.
    ///
    /// Only on request: a tone on every boot would be annoying.
    Tone,
    /// Reboot into the application.
    ///
    /// Lets a laptop restart the box while watching its console. `esptool`'s
    /// reset needs exclusive use of the serial port, so it cannot run while
    /// something is capturing the output.
    Reboot,
    /// Mount the SD card and checksum what is on it.
    ///
    /// Only on request: it powers a rail and can keep the bus busy for hours
    /// on a large card.
    Storage,
    /// Play the first WAV file on the card.
    ///
    /// Tests the path from card to speaker without the decoder.
    PlayWav,
    /// Decode and play one named content file, `CONTENT/<dir>/<file>`.
    ///
    /// Also reaches the system sounds under `00000000` to `00000003`, which
    /// are small and finish in seconds, so they make good test files.
    PlayContent { directory: u32, file: u32 },
    /// Play one of the box's own sounds, in whichever language it speaks.
    ///
    /// A file ID means the same sound in all four language directories, so
    /// only the file is named.
    PlaySound { file: u32 },
    /// Stop whatever is playing.
    Stop,
    /// Decode the first TAF and print its samples, without playing them.
    ///
    /// Silent and as fast as the decoder can go, for comparing the samples
    /// with a reference decoder on the host.
    DumpPcm { frames: u8 },
    /// Print one CSV line of pack telemetry every `seconds`, or stop if zero.
    ///
    /// Raw readings only, with no levels or smoothing. Used to measure a
    /// discharge and calibrate `BatteryConfig`.
    BatteryLog { seconds: u8 },
    /// Decode and play the first TAF file on the card.
    ///
    /// Only on request, like the other playback commands.
    PlayTaf,
    /// Bring the NFC reader up and report any tag on the plate.
    ///
    /// A plain ISO 15693 tag answers inventory; a Tonie in privacy mode does
    /// not, which looks the same as a wiring fault.
    Nfc,
    /// Remember the SLIX privacy password for this session.
    ///
    /// Typed at the console so the credential stays out of the repository.
    /// Kept in RAM only, until the next reset.
    Password(u32),
    /// Unlock a Tonie with the remembered password, then read its UID.
    Unlock,
    /// Send SET PASSWORD unconditionally, skipping the inventory that would
    /// otherwise answer first.
    ///
    /// A tag that is already unlocked answers the first inventory, so
    /// `Unlock` never sends the password. This sends it anyway, to test the
    /// password exchange (the longest frame the reader sends).
    ForceUnlock,
    /// Put a tag back into privacy mode with the remembered password.
    ///
    /// Stock firmware re-locks a figure after reading it. This puts a test tag
    /// back in that state, so the unlock path can be tested again.
    Lock,
    /// Dump a tag's memory, `count` blocks starting at `first`.
    ///
    /// The `Authorization: BD <64 hex>` token the box sends to teddyCloud is
    /// the tag's memory content. On a Tonie, `mem 00 08` reads the whole
    /// token; blocks from 8 onward do not answer.
    ///
    /// The range is typed rather than fixed, so a different tag can be
    /// explored.
    ReadMemory { first: u8, count: u8 },
    /// Ask the radio what access points it can hear.
    ///
    /// Needs no credentials, network stack or card, so an empty result points
    /// at the radio or antenna, not a password.
    NetScan,
    /// Remember the network name to associate with.
    ///
    /// Kept in RAM until the next reset, and overrides the card's
    /// `CONFIG.TXT`.
    NetSsid(String<MAX_SSID>),
    /// Remember the passphrase to associate with.
    ///
    /// Never echoed. This is why [`MAX_LINE`] is so long.
    NetPassword(String<MAX_PASSPHRASE>),
    /// Change, or remove, the passphrase of the box's own setup access point.
    ///
    /// `None` removes the key, going back to the published passphrase. Only
    /// setup mode acts on this; it is the way back into a box whose
    /// `setup_password` was forgotten.
    SetupPassword(Option<String<MAX_PASSPHRASE>>),
    /// Associate with the remembered network and take a DHCP lease.
    NetUp,
    /// Open a TLS connection to the configured server and hang up.
    ///
    /// Tests TLS on its own, separately from downloading, so a failure here
    /// points at the handshake, cipher suite or record layer.
    NetTls,
    /// Report how deep the stack has ever gone.
    ///
    /// Needed to size anything that shares RAM with the stack, such as the
    /// audio buffer.
    StackReport,
    /// Report which slot booted and what `otadata` says about it.
    ///
    /// Changes nothing. Shows from outside whether a rollback happened or the
    /// box only rebooted.
    OtaStatus,
    /// Erase, write and read back one sector of the slot that is not running.
    ///
    /// Tests whether a flash write works while Wi-Fi is connected: flash
    /// writes suspend the instruction cache while Wi-Fi interrupts are
    /// running. Writes to the inactive slot, where a real update writes.
    OtaWriteProbe,
    /// Arm the next boot on `slot`, in the state a fresh image is left in.
    ///
    /// Sets the slot and marks it pending verification, as a real update does
    /// just before rebooting. Used to test what happens when a new image
    /// fails. If the image in that slot cannot boot, the way out is J100
    /// (download mode), so the slot must be typed explicitly.
    OtaBoot { slot: u8 },
    /// Whether a held ear skips a chapter, for this session.
    ///
    /// Overrides the card's `ears_skip` until the next reset, including over
    /// a later card read.
    EarsSkip(bool),
    /// Download one content file and check it, without writing to the card.
    ///
    /// The eight bytes are the identifier **as it appears in the URL and in
    /// teddyCloud's listing** (the reversed UID), so it can be copied from the
    /// server. `request.rs` reverses it again, so the caller passes the bytes
    /// in reverse order.
    Get([u8; 8]),
    /// Read the tag's memory and keep it, to spend on a download.
    ///
    /// Unlike [`Command::ReadMemory`], this keeps the blocks and prints only
    /// their length, because they are the credential for downloading this
    /// figure's audio.
    ReadToken,
    /// Play a file a download put in `/CACHE/`.
    ///
    /// Named by the same sixteen digits used with `get`. Split into directory
    /// and file, as on the card.
    PlayCache { directory: u32, file: u32 },
    /// Checksum one file a download put in `/CACHE/`, without walking the
    /// rest of the card.
    ///
    /// `sd` computes the same CRC32 but walks the whole card, which takes
    /// hours. Named by the same sixteen digits as `get` and `play`.
    Crc { directory: u32, file: u32 },
    /// Drop the association and power the modem down.
    NetDown,
    /// Report whether the radio is up, and on what address.
    NetStatus,
    /// Override one register of the codec's start-up sequence.
    ///
    /// For finding by ear which registers cause a click at start-up, without
    /// reflashing. Overrides take effect at the next `CodecInit`, not
    /// immediately.
    CodecSet { page: u8, register: u8, value: u8 },
    /// Forget every override, back to the compiled-in sequence.
    CodecClear,
    /// Power the codec down and up again, so the start-up click can be heard
    /// without rebooting.
    CodecInit,
    /// Run the codec's software power-down and nothing else.
    CodecDown,
    /// Power the codec's output path up or down.
    ///
    /// Powering the class-D amplifier and the DAC causes the click. Playback
    /// does this automatically; this command does it by hand.
    Output(bool),
    /// Mute or unmute the class-D speaker driver, now rather than at start-up.
    ///
    /// The codec's soft-stepping only works while it has a clock, which it
    /// only has while audio plays. This allows unmuting at any time, to hear
    /// the difference.
    Speaker(bool),
    /// Say what the codec's headset-detect register reads, and what the box is
    /// routing to.
    ///
    /// The register shows what the hardware detects; the box's own routing
    /// may have been forced by the command below.
    HeadphoneStatus,
    /// Force the routing, whatever detection says.
    ///
    /// Sets both the speaker mute and the volume steps together; setting only
    /// one would leave a confusing mix. Also a manual fallback if detection
    /// fails.
    Headphones(bool),
    /// Whether the reader polls the plate on its own.
    ///
    /// **On at boot**, so the box reacts to figures without any command. A
    /// release image only accepts `dl`, so it could not turn it on.
    ///
    /// `plate off` is for tests: a reader that unlocks tags on its own would
    /// interfere with any test involving a figure, such as downloads.
    Plate(bool),
    /// Hold the idle timeout off, or let it run again.
    ///
    /// For long tests (a discharge, a download) during which the idle timeout
    /// would otherwise park the box. Off at boot.
    StayAwake(bool),
    /// Enter deep sleep now, wakeable by the ear line.
    ///
    /// Manual only. See [`Command::AutoSleep`] for the automatic path.
    Sleep,
    /// Forget which figures have been asked about, so the next placement asks
    /// the server again.
    ///
    /// Each figure is checked once per boot, because using Wi-Fi costs
    /// battery. After replacing a file on the server, this makes the box ask
    /// again without a power cycle.
    Revalidate,
    /// Whether the box may end a session in deep sleep rather than parking.
    ///
    /// Off at boot and reset on every restart, like [`Command::StayAwake`].
    /// Sleep current and the gate pins' state during sleep have not been
    /// measured, so automatic sleep must be turned on explicitly.
    AutoSleep(bool),
    /// Set `CLICK_THS` without reflashing, to tune the slap threshold. One
    /// step is full scale / 128: about 62 mg at the +/-8 g the driver sets.
    SlapThreshold { threshold: u8 },
    /// Set `TIME_LIMIT` without reflashing. Counted in sample periods (2.5 ms
    /// at the 400 Hz `init` sets), it is the longest an acceleration may stay
    /// over the threshold and still count as a click. It separates a slap
    /// from the box rocking afterwards.
    SlapTimeLimit { limit: u8 },
}

/// Longest network name accepted, in octets. 802.11 says 32.
pub const MAX_SSID: usize = 32;
/// Longest passphrase accepted. A WPA2 passphrase has up to 63 characters; a
/// 64-character value is a raw key, which is not accepted here.
pub const MAX_PASSPHRASE: usize = 63;

/// Longest command line accepted. Longer lines are discarded.
///
/// Sized for `net pw <63 characters>` (70 bytes), by far the longest command.
/// A cut-off passphrase would fail to connect with no hint why.
const MAX_LINE: usize = 72;

/// Watches a byte stream for a command line.
///
/// A command only runs on a complete line that matches exactly, because the
/// box prints status lines on its own and a terminal may echo. `ddl` is a
/// typo, not a request to reboot.
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
                    b"hp" => Some(Command::HeadphoneStatus),
                    b"hp 1" => Some(Command::Headphones(true)),
                    b"hp 0" => Some(Command::Headphones(false)),
                    b"net scan" => Some(Command::NetScan),
                    b"net up" => Some(Command::NetUp),
                    b"net tls" => Some(Command::NetTls),
                    b"token" => Some(Command::ReadToken),
                    b"stack" => Some(Command::StackReport),
                    b"otas" => Some(Command::OtaStatus),
                    b"otaw" => Some(Command::OtaWriteProbe),
                    b"net down" => Some(Command::NetDown),
                    b"net status" => Some(Command::NetStatus),
                    b"ears skip on" => Some(Command::EarsSkip(true)),
                    b"ears skip off" => Some(Command::EarsSkip(false)),
                    b"plate on" => Some(Command::Plate(true)),
                    b"plate off" => Some(Command::Plate(false)),
                    b"reval" => Some(Command::Revalidate),
                    b"sleep" => Some(Command::Sleep),
                    b"autosleep on" => Some(Command::AutoSleep(true)),
                    b"autosleep off" => Some(Command::AutoSleep(false)),
                    b"awake on" => Some(Command::StayAwake(true)),
                    b"awake off" => Some(Command::StayAwake(false)),
                    other => parse_password(other)
                        .or_else(|| parse_codec_set(other))
                        .or_else(|| parse_play_content(other))
                        .or_else(|| parse_dump_pcm(other))
                        .or_else(|| parse_battery_log(other))
                        .or_else(|| slap_time_limit(other))
                        .or_else(|| slap(other))
                        .or_else(|| parse_read_memory(other))
                        .or_else(|| parse_credential(other))
                        .or_else(|| parse_setup_password(other))
                        .or_else(|| parse_get(other))
                        .or_else(|| parse_crc(other))
                        .or_else(|| parse_ota_boot(other)),
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

/// Reads `otaboot <0|1>`, the slot to arm the next boot on.
///
/// Only `0` or `1`. This command can leave the box unbootable, so anything
/// else is not a command.
fn parse_ota_boot(line: &[u8]) -> Option<Command> {
    match line.strip_prefix(b"otaboot ")? {
        b"0" => Some(Command::OtaBoot { slot: 0 }),
        b"1" => Some(Command::OtaBoot { slot: 1 }),
        _ => None,
    }
}

/// Reads `pcm <2 hex>`, a frame count.
fn parse_dump_pcm(line: &[u8]) -> Option<Command> {
    let frames = hex_byte(line.strip_prefix(b"pcm ")?)?;
    Some(Command::DumpPcm { frames })
}

/// Reads `batlog <2 hex>`, an interval in seconds. Zero stops the log.
fn parse_battery_log(line: &[u8]) -> Option<Command> {
    let seconds = hex_byte(line.strip_prefix(b"batlog ")?)?;
    Some(Command::BatteryLog { seconds })
}

/// Reads `slapt <2 hex>`, a click time limit.
fn slap_time_limit(line: &[u8]) -> Option<Command> {
    let limit = hex_byte(line.strip_prefix(b"slapt ")?)?;
    Some(Command::SlapTimeLimit { limit })
}

/// Reads `slap <2 hex>`, a click threshold.
fn slap(line: &[u8]) -> Option<Command> {
    let threshold = hex_byte(line.strip_prefix(b"slap ")?)?;
    Some(Command::SlapThreshold { threshold })
}

/// Reads `play <8 hex>/<8 hex>`, the path the box keeps its audio under.
fn parse_play_content(line: &[u8]) -> Option<Command> {
    let rest = line.strip_prefix(b"play ")?;
    // Sixteen digits is a downloaded file's identifier, as for `get`.
    // Checked before splitting on `/`.
    if rest.len() == 16 {
        let directory = hex_u32(&rest[..8])?;
        let file = hex_u32(&rest[8..])?;
        return Some(Command::PlayCache { directory, file });
    }
    let mut halves = rest.split(|&b| b == b'/');
    let first = hex_u32(halves.next()?)?;
    // One part names a system sound in the box's language; two parts name a
    // path.
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

/// Parses `get <16 hex>` into the eight identifier bytes.
///
/// Exactly sixteen digits: a shorter identifier would name a different
/// figure, and the server's `404` would look like "no content".
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

/// Reads `crc <16 hex>`, the same identifier `get` and `play` take.
fn parse_crc(line: &[u8]) -> Option<Command> {
    let rest = line.strip_prefix(b"crc ")?;
    if rest.len() != 16 {
        return None;
    }
    let directory = hex_u32(&rest[..8])?;
    let file = hex_u32(&rest[8..])?;
    Some(Command::Crc { directory, file })
}

/// Reads exactly two hex digits.
///
/// A single digit is treated as a typo, since these values are written to
/// hardware registers.
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
    // Only the pages this firmware uses: 0, 1, and 3 (headset-detect
    // debounce clock).
    let page = match parts.next()? {
        b"0" => 0,
        b"1" => 1,
        b"3" => 3,
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
/// Limits the reply buffer. Well above the eight blocks of a Tonie, so a
/// read can go past the end.
pub const MAX_MEMORY_BLOCKS: u8 = 32;

/// Reads `setup pw off`, or `setup pw <passphrase>`.
///
/// Everything after the space is the passphrase, including `#`. `off` cannot
/// be a real passphrase: WPA2 needs at least 8 characters.
///
/// The length is not checked here. The firmware writes the value into the
/// file and parses the whole file before saving, so `teddiebox_config`
/// decides what is valid.
fn parse_setup_password(line: &[u8]) -> Option<Command> {
    let rest = line.strip_prefix(b"setup pw ")?;
    if rest == b"off" {
        return Some(Command::SetupPassword(None));
    }
    let text = core::str::from_utf8(rest).ok()?;
    String::try_from(text)
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| Command::SetupPassword(Some(s)))
}

/// Reads `net ssid <name>` and `net pw <passphrase>`.
///
/// The value is taken exactly, to the end of the line: a passphrase may
/// contain spaces. Empty or too-long values are refused, not truncated.
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
/// Hex, because block numbers come from datasheet tables.
fn parse_read_memory(line: &[u8]) -> Option<Command> {
    let rest = line.strip_prefix(b"mem ")?;
    let mut parts = rest.split(|&b| b == b' ');
    let first = hex_byte(parts.next()?)?;
    let count = hex_byte(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    // Zero is a typo. More than the buffer holds would have to be cut short,
    // and a short dump looks complete.
    if count == 0 || count > MAX_MEMORY_BLOCKS {
        return None;
    }
    Some(Command::ReadMemory { first, count })
}

/// Reads `pw <8 hex digits>`.
///
/// Exactly eight, because the password is a `u32` and a shorter value is a
/// typo. A tag answers a wrong password with silence, which looks the same as
/// an empty plate or a broken antenna.
fn parse_password(line: &[u8]) -> Option<Command> {
    let digits = line.strip_prefix(b"pw ")?;
    // The same parser the build uses for a compiled-in password.
    Some(Command::Password(crate::hex::u32_from_hex(digits)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(watch: &mut CommandWatch, bytes: &[u8]) -> Option<Command> {
        bytes.iter().find_map(|&b| watch.feed(b))
    }

    /// The way back into a box whose `setup_password` was forgotten, typed on
    /// the console the portal already runs.
    #[test]
    fn setup_pw_carries_the_passphrase_as_written() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"setup pw our house #1\n"),
            Some(Command::SetupPassword(Some(
                String::try_from("our house #1").unwrap()
            )))
        );
    }

    /// `off` puts the card back to the published passphrase. It can never
    /// collide with a real one: WPA2 will not take three characters.
    #[test]
    fn setup_pw_off_asks_for_the_key_to_be_removed() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"setup pw off\n"),
            Some(Command::SetupPassword(None))
        );
    }

    #[test]
    fn setup_pw_with_nothing_after_it_is_not_a_command() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"setup pw \n"), None);
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

    #[test]
    fn the_tone_command_is_a_single_letter() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"t\r"), Some(Command::Tone));
    }

    /// Only an exact line matches, so `ddl` does not put the box into
    /// download mode.
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

    /// `slix` stops as soon as a plain inventory answers, so on an unlocked
    /// tag it never sends the password. `slixp` always sends it.
    #[test]
    fn the_forced_unlock_command_is_distinct_from_the_unlock_one() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"slixp\r"), Some(Command::ForceUnlock));
        assert_eq!(feed_all(&mut watch, b"slix\r"), Some(Command::Unlock));
    }

    /// A locked tag stops answering, so `lock` must not be confused with
    /// `slix` or `slixp`.
    #[test]
    fn the_lock_command_is_distinct_from_the_unlock_ones() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"lock\r"), Some(Command::Lock));
        assert_eq!(feed_all(&mut watch, b"slix\r"), Some(Command::Unlock));
        assert_eq!(feed_all(&mut watch, b"slixp\r"), Some(Command::ForceUnlock));
    }

    /// Runs only the codec's software power-down, to hear whether that step
    /// is what clicks.
    #[test]
    fn the_codec_power_down_command_fires_on_its_own_line() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"cdown\r"), Some(Command::CodecDown));
    }

    /// Naming a file lets short system sounds be played as quick tests.
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

    /// One part means a system sound in the box's language.
    #[test]
    fn a_play_command_with_one_half_names_a_sound_not_a_path() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"play 00000000\r"),
            Some(Command::PlaySound { file: 0 })
        );
    }

    /// Both parts must be eight hex digits, as on the card; a shorter one
    /// would open a different file.
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

    /// Opus is not bit-exact across platforms, so the box's samples are dumped
    /// to compare how far they are from the host's, not just whether they
    /// match.
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
    fn the_batlog_command_carries_an_interval_in_seconds() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"batlog 05\r"),
            Some(Command::BatteryLog { seconds: 0x05 })
        );
    }

    /// Zero turns the log off, so it is accepted.
    #[test]
    fn a_batlog_interval_of_zero_stops_the_log() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"batlog 00\r"),
            Some(Command::BatteryLog { seconds: 0 })
        );
    }

    #[test]
    fn the_batlog_command_without_an_interval_does_not_fire() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"batlog\r"), None, "no interval");
        assert_eq!(
            feed_all(&mut watch, b"batlog 5\r"),
            None,
            "two digits, like every other count"
        );
    }

    #[test]
    fn the_slap_command_carries_a_threshold() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"slap 2d\r"),
            Some(Command::SlapThreshold { threshold: 0x2D })
        );
    }

    #[test]
    fn a_slap_command_without_a_threshold_does_not_fire() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"slap\r"), None, "no threshold");
        assert_eq!(
            feed_all(&mut watch, b"slap 5\r"),
            None,
            "one digit is not two"
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
    fn the_headphone_commands_ask_and_tell() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"hp\r"),
            Some(Command::HeadphoneStatus)
        );
        assert_eq!(
            feed_all(&mut watch, b"hp 1\r"),
            Some(Command::Headphones(true))
        );
        assert_eq!(
            feed_all(&mut watch, b"hp 0\r"),
            Some(Command::Headphones(false))
        );
        assert_eq!(feed_all(&mut watch, b"hp 2\r"), None, "not a routing");
    }

    /// Page 3 holds the headset-detect debounce clock. Pages 0 and 1 hold the
    /// start-up registers. Other pages are refused, since a mistyped page
    /// would write an unrelated register.
    #[test]
    fn cset_reaches_the_headset_debounce_clock_and_no_further() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"cset 3 10 01\r"),
            Some(Command::CodecSet {
                page: 3,
                register: 0x10,
                value: 0x01
            })
        );
        assert_eq!(
            feed_all(&mut watch, b"cset 3 10 81\r"),
            Some(Command::CodecSet {
                page: 3,
                register: 0x10,
                value: 0x81
            }),
            "the reset value must be reachable, to answer the question both ways"
        );
        assert_eq!(
            feed_all(&mut watch, b"cset 0 43 8c\r"),
            Some(Command::CodecSet {
                page: 0,
                register: 0x43,
                value: 0x8C
            }),
            "page 0 carries headset detection itself and must stay reachable"
        );
        assert_eq!(feed_all(&mut watch, b"cset 2 21 be\r"), None, "no page 2");
        assert_eq!(feed_all(&mut watch, b"cset 4 21 be\r"), None, "no page 4");
        assert_eq!(feed_all(&mut watch, b"cset 5 21 be\r"), None, "no page 5");
        assert_eq!(feed_all(&mut watch, b"cset 6 21 be\r"), None, "no page 6");
        assert_eq!(feed_all(&mut watch, b"cset 7 21 be\r"), None, "no page 7");
        assert_eq!(feed_all(&mut watch, b"cset 8 21 be\r"), None, "no page 8");
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

    /// A mistyped override must not write to a different register.
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

    /// People usually type lower case.
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

    /// A mistyped password is refused. A tag answers a wrong password with
    /// silence, which would look like a hardware fault.
    #[test]
    fn a_malformed_password_does_not_fire() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"pw DEADBEE\r"), None, "too short");
        assert_eq!(feed_all(&mut watch, b"pw DEADBEEFF\r"), None, "too long");
        assert_eq!(feed_all(&mut watch, b"pw DEADBEEG\r"), None, "not hex");
        assert_eq!(feed_all(&mut watch, b"pw\r"), None, "no value at all");
    }

    /// Status lines the box prints on its own must not look like commands.
    #[test]
    fn ordinary_traffic_does_not_fire_anything() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"teddiebox: alive 41\r\n"), None);
        assert_eq!(
            feed_all(&mut watch, b"teddiebox: accel -6912 1216 14656\r\n"),
            None
        );
    }

    /// An overlong line is discarded entirely; its end is not matched.
    #[test]
    fn an_overlong_line_cannot_match_by_its_ending() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"aaaaaaaaaaaaadl\r"), None);
    }

    /// The range is typed, not fixed, so any blocks can be read.
    #[test]
    fn a_memory_dump_names_its_first_block_and_a_count() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"mem 00 08\r"),
            Some(Command::ReadMemory { first: 0, count: 8 })
        );
    }

    /// Reading past the end of the tag's memory is allowed, to find where it
    /// stops answering.
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

    /// The reply buffer is fixed, so a larger count is refused rather than
    /// cut short.
    #[test]
    fn a_memory_dump_longer_than_the_buffer_is_refused() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"mem 00 21\r"), None);
    }

    /// A count of zero is a typo.
    #[test]
    fn a_memory_dump_of_no_blocks_is_refused() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"mem 00 00\r"), None);
    }

    /// A scan needs no credentials, so an empty result points at the radio,
    /// not a password.
    #[test]
    fn the_scan_command_asks_the_radio_what_it_can_hear() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"net scan\r"), Some(Command::NetScan));
    }

    /// `net` on its own is not a command.
    #[test]
    fn net_without_a_verb_does_not_fire() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"net\r"), None);
    }

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

    /// An SSID is at most 32 bytes (802.11); a longer one is refused, not cut
    /// short.
    #[test]
    fn an_ssid_longer_than_the_standard_allows_is_refused() {
        let mut watch = CommandWatch::new();
        let mut line = heapless::Vec::<u8, 80>::new();
        line.extend_from_slice(b"net ssid ").unwrap();
        line.extend_from_slice(&[b'a'; 33]).unwrap();
        line.push(b'\r').unwrap();
        assert_eq!(feed_all(&mut watch, &line), None);
    }

    /// A WPA2 passphrase can be 63 characters, the longest line the console
    /// accepts.
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

    /// An empty credential is a typo.
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
    /// Uses `on`/`off` like the other console switches (`plate on`,
    /// `awake on`), not the `yes`/`no` of the card.
    #[test]
    fn ears_skip_can_be_switched_from_the_console() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"ears skip off\n"),
            Some(Command::EarsSkip(false))
        );
        assert_eq!(
            feed_all(&mut watch, b"ears skip on\n"),
            Some(Command::EarsSkip(true))
        );
    }

    #[test]
    fn net_tls_is_recognised() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"net tls\n"), Some(Command::NetTls));
    }
    /// `get` takes the identifier **as it appears in the URL and in
    /// teddyCloud's listing** (the reversed UID), so it can be copied from the
    /// server. A figure with UID `E0040350503F2E1D` is listed as
    /// `1D2E3F50500304E0`.
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

    /// Too short, too long or not hex is refused. A wrong identifier would
    /// name a different figure and look like "no content".
    #[test]
    fn a_malformed_identifier_is_not_a_command() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"get 1D2E3F50500304E\n"), None);
        assert_eq!(feed_all(&mut watch, b"get 1D2E3F50500304E00\n"), None);
        assert_eq!(feed_all(&mut watch, b"get 1D2E3F50500304EZ\n"), None);
        assert_eq!(feed_all(&mut watch, b"get \n"), None);
    }
    #[test]
    fn ota_status_is_recognised() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"otas\n"), Some(Command::OtaStatus));
    }

    #[test]
    fn ota_write_probe_is_recognised() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"otaw\n"),
            Some(Command::OtaWriteProbe)
        );
    }

    /// The slot is named explicitly, not "the other one": this command can
    /// leave the box unbootable.
    #[test]
    fn ota_boot_takes_a_slot() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"otaboot 0\n"),
            Some(Command::OtaBoot { slot: 0 })
        );
        assert_eq!(
            feed_all(&mut watch, b"otaboot 1\n"),
            Some(Command::OtaBoot { slot: 1 })
        );
    }

    /// There are two slots; anything else is a typo.
    #[test]
    fn ota_boot_refuses_anything_but_a_slot() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"otaboot 2\n"), None);
        assert_eq!(feed_all(&mut watch, b"otaboot \n"), None);
        assert_eq!(feed_all(&mut watch, b"otaboot\n"), None);
        assert_eq!(feed_all(&mut watch, b"otaboot 01\n"), None);
    }

    #[test]
    fn stack_is_recognised() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"stack\n"), Some(Command::StackReport));
    }
    /// A download is stored in `/CACHE/` under the identifier `get` used, so
    /// the same sixteen digits play it.
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

    /// Eight digits is a system sound in the box's language; eight and eight
    /// is a path under `CONTENT`.
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
    /// The same sixteen digits as `get` and `play` check a download, without
    /// walking the whole card.
    #[test]
    fn crc_takes_a_ruid_to_mean_the_cache() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"crc 1A2B3C4D500304E0\n"),
            Some(Command::Crc {
                directory: 0x1A2B_3C4D,
                file: 0x5003_04E0
            })
        );
    }

    /// Too short, too long or not hex is refused, as for `get`.
    #[test]
    fn a_malformed_crc_identifier_is_not_a_command() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"crc 1A2B3C4D500304E\n"), None);
        assert_eq!(feed_all(&mut watch, b"crc 1A2B3C4D500304E00\n"), None);
        assert_eq!(feed_all(&mut watch, b"crc 1A2B3C4D500304EZ\n"), None);
        assert_eq!(feed_all(&mut watch, b"crc \n"), None);
    }

    /// Separate from `mem`: `mem` prints blocks, `token` keeps them for a
    /// download.
    #[test]
    fn token_is_recognised() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"token\n"), Some(Command::ReadToken));
    }

    /// Long tests need the idle timeout held off.
    #[test]
    fn the_idle_shutdown_can_be_held_off_from_the_console() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"awake on\r"),
            Some(Command::StayAwake(true))
        );
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"awake off\r"),
            Some(Command::StayAwake(false))
        );
    }

    /// Sleep on demand, so sleep current can be measured.
    #[test]
    fn sleep_is_its_own_command() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"sleep\r"), Some(Command::Sleep));
    }

    /// Automatic sleep is off by default and must be turned on after each
    /// reset.
    #[test]
    fn the_automatic_ending_can_be_armed_from_the_console() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"autosleep on\r"),
            Some(Command::AutoSleep(true))
        );
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"autosleep off\r"),
            Some(Command::AutoSleep(false))
        );
    }

    /// After replacing a file on the server, this makes the box ask again
    /// without a power cycle.
    #[test]
    fn the_box_can_be_told_to_ask_the_server_again() {
        let mut watch = CommandWatch::new();
        assert_eq!(feed_all(&mut watch, b"reval\r"), Some(Command::Revalidate));
    }

    #[test]
    fn the_plate_poller_can_be_switched_from_the_console() {
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"plate on\r"),
            Some(Command::Plate(true))
        );
        let mut watch = CommandWatch::new();
        assert_eq!(
            feed_all(&mut watch, b"plate off\r"),
            Some(Command::Plate(false))
        );
    }
}
