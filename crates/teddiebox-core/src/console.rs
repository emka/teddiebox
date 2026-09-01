//! The bench console: commands the box accepts on UART0.
//!
//! Line matching only, so it can be tested on the host. What a command *does*
//! belongs to the firmware, which owns the hardware to do it with.

/// A command the box understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Reboot into the ROM's UART download mode, for flashing.
    DownloadMode,
    /// Reboot into the application.
    ///
    /// Exists so a laptop can restart the box while watching its console.
    /// `esptool`'s reset takes exclusive hold of the serial port, so it cannot
    /// run while anything is capturing; writing two bytes can.
    Reboot,
}

/// Longest command line accepted. Anything longer cannot be a command, and is
/// discarded rather than allowed to shift a buffer around.
const MAX_LINE: usize = 8;

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
                    _ => None,
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
