//! The bench console: commands the box accepts on UART0.
//!
//! Byte matching only, so it can be tested on the host. What a command *does*
//! belongs to the firmware, which owns the hardware to do it with.

/// Typed at the console to reboot into ROM download mode.
///
/// Short enough to type on a bench, and terminated, so it cannot fire on a
/// prefix of ordinary output looping back into the port.
const COMMAND: &[u8] = b"dl";

/// Watches a byte stream for [`COMMAND`].
///
/// The box prints a heartbeat forever, and a terminal may echo, so this must
/// only fire on a deliberate, completed line.
#[derive(Debug, Default)]
pub struct CommandWatch {
    matched: usize,
}

impl CommandWatch {
    pub const fn new() -> Self {
        Self { matched: 0 }
    }

    /// Feeds one received byte. True exactly once, when the command completes.
    pub fn feed(&mut self, byte: u8) -> bool {
        if self.matched == COMMAND.len() {
            // The whole word is in; only a line ending completes it.
            if byte == b'\r' || byte == b'\n' {
                self.matched = 0;
                return true;
            }
            self.matched = 0;
        }

        if byte == COMMAND[self.matched] {
            self.matched += 1;
        } else {
            // Restart, but a mismatched byte may itself begin a fresh command.
            self.matched = usize::from(byte == COMMAND[0]);
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(watch: &mut CommandWatch, bytes: &[u8]) -> bool {
        bytes.iter().any(|&b| watch.feed(b))
    }

    #[test]
    fn the_command_fires_on_its_terminator() {
        let mut watch = CommandWatch::new();
        assert!(!feed_all(&mut watch, b"dl"), "not until the line ends");
        assert!(watch.feed(b'\r'));
    }

    #[test]
    fn a_newline_terminates_it_too() {
        let mut watch = CommandWatch::new();
        assert!(feed_all(&mut watch, b"dl\n"));
    }

    /// Terminals echo, users mistype, and line noise exists. Anything before
    /// the command is skipped rather than poisoning the match.
    #[test]
    fn leading_junk_does_not_prevent_a_match() {
        let mut watch = CommandWatch::new();
        assert!(feed_all(&mut watch, b"xyz dl\r"));
    }

    /// The classic matcher bug: a repeated first character must restart the
    /// match rather than desynchronising it.
    #[test]
    fn a_repeated_first_character_restarts_the_match() {
        let mut watch = CommandWatch::new();
        assert!(feed_all(&mut watch, b"ddl\r"));
    }

    #[test]
    fn an_interrupted_command_does_not_fire() {
        let mut watch = CommandWatch::new();
        assert!(!feed_all(&mut watch, b"d\rl\r"));
    }

    /// A heartbeat prints once a second forever; the watcher must not fire on
    /// ordinary output looping back or on any prefix of it.
    #[test]
    fn ordinary_traffic_does_not_fire_it() {
        let mut watch = CommandWatch::new();
        assert!(!feed_all(&mut watch, b"teddiebox: alive 41\r\n"));
    }
}
