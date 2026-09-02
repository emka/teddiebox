//! The bench console: commands the box accepts on UART0.
//!
//! Line matching only, so it can be tested on the host. What a command *does*
//! belongs to the firmware, which owns the hardware to do it with.

/// A command the box understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

/// Longest command line accepted. Anything longer cannot be a command, and is
/// discarded rather than allowed to shift a buffer around.
///
/// Sixteen rather than eight since `pw` carries eight hex digits after it.
const MAX_LINE: usize = 16;

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
                    b"nfc" => Some(Command::Nfc),
                    b"slix" => Some(Command::Unlock),
                    b"slixp" => Some(Command::ForceUnlock),
                    other => parse_password(other),
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
}
