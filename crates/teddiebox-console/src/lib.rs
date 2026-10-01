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
    /// Print one CSV line of battery telemetry every `seconds`, or stop if zero.
    ///
    /// Raw readings only, with no levels or smoothing. Used to measure a
    /// discharge and calibrate `BatteryConfig`.
    BatteryLog { seconds: u8 },
    /// Decode and play the first TAF file on the card.
    ///
    /// Only on request, like the other playback commands.
    PlayTaf,
    /// Bring the NFC reader up and report any tag on the box.
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
    /// Restart into setup mode, as if both ears were held at power-on.
    ///
    /// For the bench, where nobody may be at the box to hold them.
    EnterSetup,
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
    /// Whether the reader polls the box on its own.
    ///
    /// **On at boot**, so the box reacts to figures without any command. A
    /// release image only accepts `dl`, so it could not turn it on.
    ///
    /// `plate off` is for tests: a reader that unlocks tags on its own would
    /// interfere with any test involving a figure, such as downloads.
    Plate(bool),
    /// Checksum every decoded frame of a story, or stop.
    ///
    /// **Off at boot.** The checksum lets a bench run compare the box's decode
    /// with `taf2wav` on the host, but computed bit by bit it takes a fifth of
    /// playback time, which leaves the loop too little margin to keep the DMA
    /// fed.
    PcmCrc(bool),
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
                    b"setup" => Some(Command::EnterSetup),
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
                    b"pcmcrc on" => Some(Command::PcmCrc(true)),
                    b"pcmcrc off" => Some(Command::PcmCrc(false)),
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
        let directory = crate::hex::u32_from_hex(&rest[..8])?;
        let file = crate::hex::u32_from_hex(&rest[8..])?;
        return Some(Command::PlayCache { directory, file });
    }
    let mut halves = rest.split(|&b| b == b'/');
    let first = crate::hex::u32_from_hex(halves.next()?)?;
    // One part names a system sound in the box's language; two parts name a
    // path.
    let Some(second) = halves.next() else {
        return Some(Command::PlaySound { file: first });
    };
    let file = crate::hex::u32_from_hex(second)?;
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
    let directory = crate::hex::u32_from_hex(&rest[..8])?;
    let file = crate::hex::u32_from_hex(&rest[8..])?;
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
/// a box with no figure or a broken antenna.
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

    /// Each line fed in turn to one watch, with the command it fired, if any.
    fn fired<const N: usize>(lines: [&'static str; N]) -> [(&'static str, Option<Command>); N] {
        let mut watch = CommandWatch::new();
        lines.map(|line| (line, feed_all(&mut watch, line.as_bytes())))
    }

    /// The way back into a box whose `setup_password` was forgotten, typed on
    /// the console the portal already runs.
    #[test]
    fn setup_pw_carries_the_passphrase_as_written() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"setup pw our house #1\n");

        // Then
        assert_eq!(
            parsed,
            Some(Command::SetupPassword(Some(
                String::try_from("our house #1").unwrap()
            )))
        );
    }

    /// `off` puts the card back to the published passphrase. It can never
    /// collide with a real one: WPA2 will not take three characters.
    #[test]
    fn setup_pw_off_asks_for_the_key_to_be_removed() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"setup pw off\n");

        // Then
        assert_eq!(parsed, Some(Command::SetupPassword(None)));
    }

    #[test]
    fn setup_asks_for_setup_mode() {
        // Given
        // Given
        let mut watch = CommandWatch::new();

        // When

        // When
        let command = feed_all(&mut watch, b"setup\n");

        // Then
        // Then
        assert_eq!(command, Some(Command::EnterSetup));
    }

    #[test]
    fn setup_pw_is_not_mistaken_for_setup() {
        // Given
        // Given
        let mut watch = CommandWatch::new();

        // When

        // When
        let command = feed_all(&mut watch, b"setup pw off\n");

        // Then
        // Then
        assert_ne!(command, Some(Command::EnterSetup));
    }

    #[test]
    fn setup_pw_with_nothing_after_it_is_not_a_command() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"setup pw \n");

        // Then
        assert_eq!(parsed, None);
    }

    #[test]
    fn the_download_command_fires_on_its_terminator() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let before_the_end = feed_all(&mut watch, b"dl");
        let at_the_end = watch.feed(b'\r');

        // Then
        assert_eq!(before_the_end, None, "not until the line ends");
        assert_eq!(at_the_end, Some(Command::DownloadMode));
    }

    #[test]
    fn the_reboot_command_is_distinct() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"rb\n");

        // Then
        assert_eq!(parsed, Some(Command::Reboot));
    }

    #[test]
    fn the_tone_command_is_a_single_letter() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"t\r");

        // Then
        assert_eq!(parsed, Some(Command::Tone));
    }

    /// Only an exact line matches, so `ddl` does not put the box into
    /// download mode.
    #[test]
    fn a_mistyped_line_does_not_fire() {
        // Given
        let lines = ["ddl\r", "dl \r", "DL\r"];

        // When
        let commands = fired(lines);

        // Then
        assert_eq!(
            commands,
            [("ddl\r", None), ("dl \r", None), ("DL\r", None),]
        );
    }

    #[test]
    fn a_correct_line_after_a_wrong_one_still_fires() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"nonsense\rdl\r");

        // Then
        assert_eq!(parsed, Some(Command::DownloadMode));
    }

    #[test]
    fn the_storage_command_fires_on_its_own_line() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"sd\r");

        // Then
        assert_eq!(parsed, Some(Command::Storage));
    }

    #[test]
    fn the_wav_command_fires_on_its_own_line() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"wav\r");

        // Then
        assert_eq!(parsed, Some(Command::PlayWav));
    }

    #[test]
    fn the_taf_command_is_distinct_from_the_wav_one() {
        // Given
        let lines = ["taf\r", "wav\r"];

        // When
        let commands = fired(lines);

        // Then
        assert_eq!(
            commands,
            [
                ("taf\r", Some(Command::PlayTaf)),
                ("wav\r", Some(Command::PlayWav)),
            ]
        );
    }

    #[test]
    fn the_nfc_command_fires_on_its_own_line() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"nfc\r");

        // Then
        assert_eq!(parsed, Some(Command::Nfc));
    }

    #[test]
    fn the_unlock_command_is_distinct_from_the_nfc_one() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"slix\r");

        // Then
        assert_eq!(parsed, Some(Command::Unlock));
    }

    /// `slix` stops as soon as a plain inventory answers, so on an unlocked
    /// tag it never sends the password. `slixp` always sends it.
    #[test]
    fn the_forced_unlock_command_is_distinct_from_the_unlock_one() {
        // Given
        let lines = ["slixp\r", "slix\r"];

        // When
        let commands = fired(lines);

        // Then
        assert_eq!(
            commands,
            [
                ("slixp\r", Some(Command::ForceUnlock)),
                ("slix\r", Some(Command::Unlock)),
            ]
        );
    }

    /// A locked tag stops answering, so `lock` must not be confused with
    /// `slix` or `slixp`.
    #[test]
    fn the_lock_command_is_distinct_from_the_unlock_ones() {
        // Given
        let lines = ["lock\r", "slix\r", "slixp\r"];

        // When
        let commands = fired(lines);

        // Then
        assert_eq!(
            commands,
            [
                ("lock\r", Some(Command::Lock)),
                ("slix\r", Some(Command::Unlock)),
                ("slixp\r", Some(Command::ForceUnlock)),
            ]
        );
    }

    /// Runs only the codec's software power-down, to hear whether that step
    /// is what clicks.
    #[test]
    fn the_codec_power_down_command_fires_on_its_own_line() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"cdown\r");

        // Then
        assert_eq!(parsed, Some(Command::CodecDown));
    }

    /// Naming a file lets short system sounds be played as quick tests.
    #[test]
    fn the_play_command_carries_the_content_id_it_names() {
        // Given
        let lines = ["play 00000000/00000003\r", "play 1A2B3C4D/500304E0\r"];

        // When
        let commands = fired(lines);

        // Then
        assert_eq!(
            commands,
            [
                (
                    "play 00000000/00000003\r",
                    Some(Command::PlayContent {
                        directory: 0x0000_0000,
                        file: 0x0000_0003
                    })
                ),
                (
                    "play 1A2B3C4D/500304E0\r",
                    Some(Command::PlayContent {
                        directory: 0x1A2B_3C4D,
                        file: 0x5003_04E0
                    })
                ),
            ]
        );
    }

    /// One part means a system sound in the box's language.
    #[test]
    fn a_play_command_with_one_half_names_a_sound_not_a_path() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"play 00000000\r");

        // Then
        assert_eq!(parsed, Some(Command::PlaySound { file: 0 }));
    }

    /// Both parts must be eight hex digits, as on the card; a shorter one
    /// would open a different file.
    #[test]
    fn a_malformed_content_id_does_not_fire() {
        // Given
        let lines = ["play 0000000/00000003\r", "play 0000000G/00000003\r"];

        // When
        let commands = fired(lines);

        // Then
        assert_eq!(
            commands,
            [
                // short
                ("play 0000000/00000003\r", None),
                // not hex
                ("play 0000000G/00000003\r", None),
            ]
        );
    }

    /// Opus is not bit-exact across platforms, so the box's samples are dumped
    /// to compare how far they are from the host's, not just whether they
    /// match.
    #[test]
    fn the_pcm_command_carries_how_many_frames_to_dump() {
        // Given
        let lines = ["pcm 53\r", "pcm ff\r", "pcm\r", "pcm 5\r"];

        // When
        let commands = fired(lines);

        // Then
        assert_eq!(
            commands,
            [
                ("pcm 53\r", Some(Command::DumpPcm { frames: 0x53 })),
                ("pcm ff\r", Some(Command::DumpPcm { frames: 0xFF })),
                // no count
                ("pcm\r", None),
                // one digit is a typo
                ("pcm 5\r", None),
            ]
        );
    }

    #[test]
    fn the_batlog_command_carries_an_interval_in_seconds() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"batlog 05\r");

        // Then
        assert_eq!(parsed, Some(Command::BatteryLog { seconds: 0x05 }));
    }

    /// Zero turns the log off, so it is accepted.
    #[test]
    fn a_batlog_interval_of_zero_stops_the_log() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"batlog 00\r");

        // Then
        assert_eq!(parsed, Some(Command::BatteryLog { seconds: 0 }));
    }

    #[test]
    fn the_batlog_command_without_an_interval_does_not_fire() {
        // Given
        let lines = ["batlog\r", "batlog 5\r"];

        // When
        let commands = fired(lines);

        // Then
        assert_eq!(
            commands,
            [
                // no interval
                ("batlog\r", None),
                // two digits, like every other count
                ("batlog 5\r", None),
            ]
        );
    }

    #[test]
    fn the_slap_command_carries_a_threshold() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"slap 2d\r");

        // Then
        assert_eq!(parsed, Some(Command::SlapThreshold { threshold: 0x2D }));
    }

    /// How long a click may last, tuned at the bench like the threshold.
    #[test]
    fn the_slap_time_limit_command_carries_its_limit() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"slapt 0a\r");

        // Then
        assert_eq!(parsed, Some(Command::SlapTimeLimit { limit: 10 }));
    }

    #[test]
    fn a_slap_command_without_a_threshold_does_not_fire() {
        // Given
        let lines = ["slap\r", "slap 5\r"];

        // When
        let commands = fired(lines);

        // Then
        assert_eq!(
            commands,
            [
                // no threshold
                ("slap\r", None),
                // one digit is not two
                ("slap 5\r", None),
            ]
        );
    }

    #[test]
    fn the_stop_command_fires_on_its_own_line() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"stop\r");

        // Then
        assert_eq!(parsed, Some(Command::Stop));
    }

    #[test]
    fn the_output_command_carries_which_way_it_goes() {
        // Given
        let lines = ["out 1\r", "out 0\r", "out\r"];

        // When
        let commands = fired(lines);

        // Then
        assert_eq!(
            commands,
            [
                ("out 1\r", Some(Command::Output(true))),
                ("out 0\r", Some(Command::Output(false))),
                // no direction given
                ("out\r", None),
            ]
        );
    }

    #[test]
    fn the_speaker_command_carries_which_way_it_goes() {
        // Given
        let lines = ["spk 1\r", "spk 0\r", "spk\r", "spk 2\r"];

        // When
        let commands = fired(lines);

        // Then
        assert_eq!(
            commands,
            [
                ("spk 1\r", Some(Command::Speaker(true))),
                ("spk 0\r", Some(Command::Speaker(false))),
                // no direction given
                ("spk\r", None),
                // not a direction
                ("spk 2\r", None),
            ]
        );
    }

    #[test]
    fn the_headphone_commands_ask_and_tell() {
        // Given
        let lines = ["hp\r", "hp 1\r", "hp 0\r", "hp 2\r"];

        // When
        let commands = fired(lines);

        // Then
        assert_eq!(
            commands,
            [
                ("hp\r", Some(Command::HeadphoneStatus)),
                ("hp 1\r", Some(Command::Headphones(true))),
                ("hp 0\r", Some(Command::Headphones(false))),
                // not a routing
                ("hp 2\r", None),
            ]
        );
    }

    /// Page 3 holds the headset-detect debounce clock. Pages 0 and 1 hold the
    /// start-up registers. Other pages are refused, since a mistyped page
    /// would write an unrelated register.
    #[test]
    fn cset_reaches_the_headset_debounce_clock_and_no_further() {
        // Given
        let lines = [
            "cset 3 10 01\r",
            "cset 3 10 81\r",
            "cset 0 43 8c\r",
            "cset 2 21 be\r",
            "cset 4 21 be\r",
            "cset 5 21 be\r",
            "cset 6 21 be\r",
            "cset 7 21 be\r",
            "cset 8 21 be\r",
        ];

        // When
        let commands = fired(lines);

        // Then
        assert_eq!(
            commands,
            [
                (
                    "cset 3 10 01\r",
                    Some(Command::CodecSet {
                        page: 3,
                        register: 0x10,
                        value: 0x01
                    })
                ),
                // the reset value must be reachable, to answer the question both ways
                (
                    "cset 3 10 81\r",
                    Some(Command::CodecSet {
                        page: 3,
                        register: 0x10,
                        value: 0x81
                    })
                ),
                // page 0 carries headset detection itself and must stay reachable
                (
                    "cset 0 43 8c\r",
                    Some(Command::CodecSet {
                        page: 0,
                        register: 0x43,
                        value: 0x8C
                    })
                ),
                // no page 2
                ("cset 2 21 be\r", None),
                // no page 4
                ("cset 4 21 be\r", None),
                // no page 5
                ("cset 5 21 be\r", None),
                // no page 6
                ("cset 6 21 be\r", None),
                // no page 7
                ("cset 7 21 be\r", None),
                // no page 8
                ("cset 8 21 be\r", None),
            ]
        );
    }

    #[test]
    fn a_codec_override_carries_its_page_register_and_value() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"cset 1 21 be\r");

        // Then
        assert_eq!(
            parsed,
            Some(Command::CodecSet {
                page: 1,
                register: 0x21,
                value: 0xBE
            })
        );
    }

    #[test]
    fn the_codec_rerun_and_clear_commands_fire_on_their_own_lines() {
        // Given
        let lines = ["cinit\r", "cclr\r"];

        // When
        let commands = fired(lines);

        // Then
        assert_eq!(
            commands,
            [
                ("cinit\r", Some(Command::CodecInit)),
                ("cclr\r", Some(Command::CodecClear)),
            ]
        );
    }

    /// A mistyped override must not write to a different register.
    #[test]
    fn a_malformed_codec_override_does_not_fire() {
        // Given
        let lines = [
            "cset 1 21\r",
            "cset 1 2 be\r",
            "cset 9 21 be\r",
            "cset 1 2g be\r",
        ];

        // When
        let commands = fired(lines);

        // Then
        assert_eq!(
            commands,
            [
                // no value
                ("cset 1 21\r", None),
                // short register
                ("cset 1 2 be\r", None),
                // no such page
                ("cset 9 21 be\r", None),
                // not hex
                ("cset 1 2g be\r", None),
            ]
        );
    }

    #[test]
    fn the_password_command_carries_its_value() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"pw DEADBEEF\r");

        // Then
        assert_eq!(parsed, Some(Command::Password(0xDEAD_BEEF)));
    }

    /// People usually type lower case.
    #[test]
    fn a_password_in_lower_case_is_accepted() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"pw deadbeef\r");

        // Then
        assert_eq!(parsed, Some(Command::Password(0xDEAD_BEEF)));
    }

    /// Leading zeroes are part of the value, not decoration: a password of
    /// 0x0000FFFF must not be read as 0xFFFF0000 or refused.
    #[test]
    fn a_password_with_leading_zeroes_keeps_its_width() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"pw 0000ffff\r");

        // Then
        assert_eq!(parsed, Some(Command::Password(0x0000_FFFF)));
    }

    /// A mistyped password is refused. A tag answers a wrong password with
    /// silence, which would look like a hardware fault.
    #[test]
    fn a_malformed_password_does_not_fire() {
        // Given
        let lines = ["pw DEADBEE\r", "pw DEADBEEFF\r", "pw DEADBEEG\r", "pw\r"];

        // When
        let commands = fired(lines);

        // Then
        assert_eq!(
            commands,
            [
                // too short
                ("pw DEADBEE\r", None),
                // too long
                ("pw DEADBEEFF\r", None),
                // not hex
                ("pw DEADBEEG\r", None),
                // no value at all
                ("pw\r", None),
            ]
        );
    }

    /// Status lines the box prints on its own must not look like commands.
    #[test]
    fn ordinary_traffic_does_not_fire_anything() {
        // Given
        let lines = [
            "teddiebox: alive 41\r\n",
            "teddiebox: accel -6912 1216 14656\r\n",
        ];

        // When
        let commands = fired(lines);

        // Then
        assert_eq!(
            commands,
            [
                ("teddiebox: alive 41\r\n", None),
                ("teddiebox: accel -6912 1216 14656\r\n", None),
            ]
        );
    }

    /// An overlong line is discarded entirely; its end is not matched.
    #[test]
    fn an_overlong_line_cannot_match_by_its_ending() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"aaaaaaaaaaaaadl\r");

        // Then
        assert_eq!(parsed, None);
    }

    /// The range is typed, not fixed, so any blocks can be read.
    #[test]
    fn a_memory_dump_names_its_first_block_and_a_count() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"mem 00 08\r");

        // Then
        assert_eq!(parsed, Some(Command::ReadMemory { first: 0, count: 8 }));
    }

    /// Reading past the end of the tag's memory is allowed, to find where it
    /// stops answering.
    #[test]
    fn a_memory_dump_may_start_past_the_expected_end() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"mem 1f 01\r");

        // Then
        assert_eq!(
            parsed,
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
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"mem 00 21\r");

        // Then
        assert_eq!(parsed, None);
    }

    /// The limit is the buffer's size, so a dump that fills it exactly is
    /// still asked for.
    #[test]
    fn a_memory_dump_that_fills_the_buffer_exactly_is_a_command() {
        // Given: 0x20 is the 32 blocks the buffer holds
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"mem 00 20\r");

        // Then
        assert_eq!(
            parsed,
            Some(Command::ReadMemory {
                first: 0,
                count: 32
            })
        );
    }

    /// A count of zero is a typo.
    #[test]
    fn a_memory_dump_of_no_blocks_is_refused() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"mem 00 00\r");

        // Then
        assert_eq!(parsed, None);
    }

    /// A scan needs no credentials, so an empty result points at the radio,
    /// not a password.
    #[test]
    fn the_scan_command_asks_the_radio_what_it_can_hear() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"net scan\r");

        // Then
        assert_eq!(parsed, Some(Command::NetScan));
    }

    /// `net` on its own is not a command.
    #[test]
    fn net_without_a_verb_does_not_fire() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"net\r");

        // Then
        assert_eq!(parsed, None);
    }

    #[test]
    fn the_ssid_command_carries_the_name_it_names() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"net ssid example-ssid\r");

        // Then
        assert_eq!(
            parsed,
            Some(Command::NetSsid(
                heapless::String::try_from("example-ssid").unwrap()
            ))
        );
    }

    /// An SSID is at most 32 bytes (802.11); a longer one is refused, not cut
    /// short.
    #[test]
    fn an_ssid_longer_than_the_standard_allows_is_refused() {
        // Given
        let mut watch = CommandWatch::new();
        let mut line = heapless::Vec::<u8, 80>::new();
        line.extend_from_slice(b"net ssid ").unwrap();
        line.extend_from_slice(&[b'a'; 33]).unwrap();
        line.push(b'\r').unwrap();

        // When
        let parsed = feed_all(&mut watch, &line);

        // Then
        assert_eq!(parsed, None);
    }

    /// A WPA2 passphrase can be 63 characters, the longest line the console
    /// accepts.
    #[test]
    fn a_passphrase_of_the_full_length_still_fits_a_line() {
        // Given
        let mut watch = CommandWatch::new();
        let mut line = heapless::Vec::<u8, 80>::new();
        line.extend_from_slice(b"net pw ").unwrap();
        line.extend_from_slice(&[b'x'; 63]).unwrap();
        line.push(b'\r').unwrap();
        let raw = [b'x'; 63];
        let expected = core::str::from_utf8(&raw).unwrap();

        // When
        let parsed = feed_all(&mut watch, &line);

        // Then
        assert_eq!(
            parsed,
            Some(Command::NetPassword(
                heapless::String::try_from(expected).unwrap()
            ))
        );
    }

    /// An empty credential is a typo.
    #[test]
    fn an_empty_ssid_is_refused() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"net ssid \r");

        // Then
        assert_eq!(parsed, None);
    }

    #[test]
    fn the_radio_can_be_asked_up_down_and_for_its_state() {
        // Given
        let lines = ["net up\r", "net down\r", "net status\r"];

        // When
        let commands = fired(lines);

        // Then
        assert_eq!(
            commands,
            [
                ("net up\r", Some(Command::NetUp)),
                ("net down\r", Some(Command::NetDown)),
                ("net status\r", Some(Command::NetStatus)),
            ]
        );
    }
    /// Uses `on`/`off` like the other console switches (`plate on`,
    /// `awake on`), not the `yes`/`no` of the card.
    #[test]
    fn ears_skip_can_be_switched_from_the_console() {
        // Given
        let lines = ["ears skip off\n", "ears skip on\n"];

        // When
        let commands = fired(lines);

        // Then
        assert_eq!(
            commands,
            [
                ("ears skip off\n", Some(Command::EarsSkip(false))),
                ("ears skip on\n", Some(Command::EarsSkip(true))),
            ]
        );
    }

    #[test]
    fn net_tls_is_recognised() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"net tls\n");

        // Then
        assert_eq!(parsed, Some(Command::NetTls));
    }
    /// `get` takes the identifier **as it appears in the URL and in
    /// teddyCloud's listing** (the reversed UID), so it can be copied from the
    /// server. A figure with UID `E0040350503F2E1D` is listed as
    /// `1D2E3F50500304E0`.
    #[test]
    fn get_takes_the_reversed_uid_as_it_appears_on_the_server() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"get 1D2E3F50500304E0\n");

        // Then
        assert_eq!(
            parsed,
            Some(Command::Get([
                0x1D, 0x2E, 0x3F, 0x50, 0x50, 0x03, 0x04, 0xE0
            ]))
        );
    }

    #[test]
    fn get_accepts_lower_case_hex() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"get 1a2b3c4d500304e0\n");

        // Then
        assert_eq!(
            parsed,
            Some(Command::Get([
                0x1A, 0x2B, 0x3C, 0x4D, 0x50, 0x03, 0x04, 0xE0
            ]))
        );
    }

    /// Too short, too long or not hex is refused. A wrong identifier would
    /// name a different figure and look like "no content".
    #[test]
    fn a_malformed_identifier_is_not_a_command() {
        // Given
        let lines = [
            "get 1D2E3F50500304E\n",
            "get 1D2E3F50500304E00\n",
            "get 1D2E3F50500304EZ\n",
            "get \n",
        ];

        // When
        let commands = fired(lines);

        // Then
        assert_eq!(
            commands,
            [
                ("get 1D2E3F50500304E\n", None),
                ("get 1D2E3F50500304E00\n", None),
                ("get 1D2E3F50500304EZ\n", None),
                ("get \n", None),
            ]
        );
    }
    #[test]
    fn ota_status_is_recognised() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"otas\n");

        // Then
        assert_eq!(parsed, Some(Command::OtaStatus));
    }

    #[test]
    fn ota_write_probe_is_recognised() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"otaw\n");

        // Then
        assert_eq!(parsed, Some(Command::OtaWriteProbe));
    }

    /// The slot is named explicitly, not "the other one": this command can
    /// leave the box unbootable.
    #[test]
    fn ota_boot_takes_a_slot() {
        // Given
        let lines = ["otaboot 0\n", "otaboot 1\n"];

        // When
        let commands = fired(lines);

        // Then
        assert_eq!(
            commands,
            [
                ("otaboot 0\n", Some(Command::OtaBoot { slot: 0 })),
                ("otaboot 1\n", Some(Command::OtaBoot { slot: 1 })),
            ]
        );
    }

    /// There are two slots; anything else is a typo.
    #[test]
    fn ota_boot_refuses_anything_but_a_slot() {
        // Given
        let lines = ["otaboot 2\n", "otaboot \n", "otaboot\n", "otaboot 01\n"];

        // When
        let commands = fired(lines);

        // Then
        assert_eq!(
            commands,
            [
                ("otaboot 2\n", None),
                ("otaboot \n", None),
                ("otaboot\n", None),
                ("otaboot 01\n", None),
            ]
        );
    }

    #[test]
    fn stack_is_recognised() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"stack\n");

        // Then
        assert_eq!(parsed, Some(Command::StackReport));
    }
    /// A download is stored in `/CACHE/` under the identifier `get` used, so
    /// the same sixteen digits play it.
    #[test]
    fn play_takes_a_ruid_to_mean_the_cache() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"play 1A2B3C4D500304E0\n");

        // Then
        assert_eq!(
            parsed,
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
        // Given
        let lines = ["play 00000010\n", "play 00000001/00000000\n"];

        // When
        let commands = fired(lines);

        // Then
        assert_eq!(
            commands,
            [
                ("play 00000010\n", Some(Command::PlaySound { file: 0x10 })),
                (
                    "play 00000001/00000000\n",
                    Some(Command::PlayContent {
                        directory: 1,
                        file: 0
                    })
                ),
            ]
        );
    }
    /// The same sixteen digits as `get` and `play` check a download, without
    /// walking the whole card.
    #[test]
    fn crc_takes_a_ruid_to_mean_the_cache() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"crc 1A2B3C4D500304E0\n");

        // Then
        assert_eq!(
            parsed,
            Some(Command::Crc {
                directory: 0x1A2B_3C4D,
                file: 0x5003_04E0
            })
        );
    }

    /// Too short, too long or not hex is refused, as for `get`.
    #[test]
    fn a_malformed_crc_identifier_is_not_a_command() {
        // Given
        let lines = [
            "crc 1A2B3C4D500304E\n",
            "crc 1A2B3C4D500304E00\n",
            "crc 1A2B3C4D500304EZ\n",
            "crc \n",
        ];

        // When
        let commands = fired(lines);

        // Then
        assert_eq!(
            commands,
            [
                ("crc 1A2B3C4D500304E\n", None),
                ("crc 1A2B3C4D500304E00\n", None),
                ("crc 1A2B3C4D500304EZ\n", None),
                ("crc \n", None),
            ]
        );
    }

    /// Separate from `mem`: `mem` prints blocks, `token` keeps them for a
    /// download.
    #[test]
    fn token_is_recognised() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"token\n");

        // Then
        assert_eq!(parsed, Some(Command::ReadToken));
    }

    /// Long tests need the idle timeout held off.
    #[test]
    fn the_idle_shutdown_can_be_held_off_from_the_console() {
        // Given
        let lines = ["awake on\r", "awake off\r"];

        // When
        let commands = fired(lines);

        // Then
        assert_eq!(
            commands,
            [
                ("awake on\r", Some(Command::StayAwake(true))),
                ("awake off\r", Some(Command::StayAwake(false)))
            ]
        );
    }

    /// Sleep on demand, so sleep current can be measured.
    #[test]
    fn sleep_is_its_own_command() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"sleep\r");

        // Then
        assert_eq!(parsed, Some(Command::Sleep));
    }

    /// Automatic sleep is off by default and must be turned on after each
    /// reset.
    #[test]
    fn the_automatic_ending_can_be_armed_from_the_console() {
        // Given
        let lines = ["autosleep on\r", "autosleep off\r"];

        // When
        let commands = fired(lines);

        // Then
        assert_eq!(
            commands,
            [
                ("autosleep on\r", Some(Command::AutoSleep(true))),
                ("autosleep off\r", Some(Command::AutoSleep(false)))
            ]
        );
    }

    /// After replacing a file on the server, this makes the box ask again
    /// without a power cycle.
    #[test]
    fn the_box_can_be_told_to_ask_the_server_again() {
        // Given
        let mut watch = CommandWatch::new();

        // When
        let parsed = feed_all(&mut watch, b"reval\r");

        // Then
        assert_eq!(parsed, Some(Command::Revalidate));
    }

    #[test]
    fn the_playback_checksum_can_be_switched_from_the_console() {
        // Given
        let lines = ["pcmcrc on\r", "pcmcrc off\r"];

        // When
        let commands = fired(lines);

        // Then
        assert_eq!(
            commands,
            [
                ("pcmcrc on\r", Some(Command::PcmCrc(true))),
                ("pcmcrc off\r", Some(Command::PcmCrc(false)))
            ]
        );
    }

    #[test]
    fn the_plate_poller_can_be_switched_from_the_console() {
        // Given
        let lines = ["plate on\r", "plate off\r"];

        // When
        let commands = fired(lines);

        // Then
        assert_eq!(
            commands,
            [
                ("plate on\r", Some(Command::Plate(true))),
                ("plate off\r", Some(Command::Plate(false)))
            ]
        );
    }
}
