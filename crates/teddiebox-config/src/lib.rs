#![no_std]

//! Parses the box's configuration file from the SD card.
//!
//! Format is deliberately the dullest thing that works: `key = value`, one per
//! line, `#` comments, blank lines ignored. A parent editing this file on a
//! laptop should not be able to get the syntax wrong.
//!
//! A `#` that starts a word ends the line, so `server = box.lan:8080 # ours`
//! means what it looks like, while `ssid = net#1` keeps its hash. **`password`
//! is exempt**: it is opaque bytes, `#` is common in them, and a password
//! truncated by a comment rule fails at the box where the cause is invisible.
//! Everything after `password =` is the password.
//!
//! Unknown keys are ignored, so a card written for a newer firmware still boots
//! an older one. A *known* key given a value it does not accept is refused —
//! `insecure = ture` is a typo about certificate checking, and the box saying so
//! beats the box guessing.

use heapless::String;

/// The file's name in the card's root directory.
///
/// Short and upper case, so that `embedded-sdmmc` opens it with the ordinary
/// `open_file_in_dir` — which takes a `ShortFileName` and refuses the ninth
/// character of a stem. A long name such as `teddiebox.conf` is not out of
/// reach: `open_long_name_file_in_dir` opens one. It just costs more than the
/// name is worth — that call rescans the directory reassembling long names,
/// cannot create a file, and would be a second way into the filesystem for the
/// media task to own, while `Storage::open_file` already speaks short names.
/// `tools/fat-assumptions` runs both doors against the real library.
///
/// `.TXT` over `.CNF` so that the laptop this gets edited on opens it in a text
/// editor rather than asking what a `.CNF` is.
pub const FILENAME: &str = "CONFIG.TXT";

pub const MAX_SSID: usize = 32;
pub const MAX_PASSWORD: usize = 63;
pub const MAX_SERVER: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub ssid: String<MAX_SSID>,
    pub password: String<MAX_PASSWORD>,
    /// `host:port` of the teddyCloud server.
    pub server: String<MAX_SERVER>,
    /// Accept the server's certificate without checking it.
    ///
    /// **An escape hatch, not the plan.** teddyCloud serves a certificate
    /// signed by its own root and hands that root over in the chain, so the
    /// ordinary answer is to trust that root and check against it.
    ///
    /// What this exists for is the box's missing clock. Validity dates cannot
    /// be checked without one, and the box's time starts at boot: a box that
    /// believes it is 1970 is *before* the window of a certificate issued in
    /// 2004, and would refuse a good one. Until the box learns the time, this
    /// is what gets it talking.
    ///
    /// Defaults to `false`. A file that says nothing gets the checking.
    pub insecure: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    MissingSsid,
    MissingServer,
    ValueTooLong,
    MalformedLine,
    /// A known key was given a value it does not accept.
    MalformedValue,
    /// The read filled its buffer, so the file may have been cut short.
    Truncated,
    /// The bytes are not UTF-8, so they are not this file.
    NotText,
}

/// Cuts a trailing `# comment` off a value.
///
/// The `#` must begin a word — preceded by whitespace, or first in the value —
/// so that `net#1` survives intact while `net #1` does not. Callers decide
/// whether a value is eligible; `password` is not.
fn strip_comment(value: &str) -> &str {
    let mut after_space = true;
    for (i, c) in value.char_indices() {
        if c == '#' && after_space {
            return value[..i].trim_end();
        }
        after_space = c.is_whitespace();
    }
    value
}

/// Reads the one boolean this file has.
///
/// `yes`/`no` and `true`/`false`, in any case, because both spellings are what
/// people reach for. Anything else is refused rather than defaulted: an
/// unrecognised value means the line did not do what it was meant to, and the
/// line in question decides whether certificates get checked. Being told at the
/// box beats a silent guess in either direction.
fn parse_bool(value: &str) -> Result<bool, ConfigError> {
    // `eq_ignore_ascii_case` compares in place; there is no allocator here.
    if value.eq_ignore_ascii_case("yes") || value.eq_ignore_ascii_case("true") {
        Ok(true)
    } else if value.eq_ignore_ascii_case("no") || value.eq_ignore_ascii_case("false") {
        Ok(false)
    } else {
        Err(ConfigError::MalformedValue)
    }
}

impl Config {
    /// Parses a file that was read into a fixed buffer.
    ///
    /// `capacity` is how large that buffer was. **A read that filled it
    /// exactly is refused**, because nothing distinguishes a file that just
    /// fits from one that was cut off — and a config cut mid-line is the
    /// dangerous kind of wrong. It still parses; it just parses into a
    /// plausible-looking value nobody typed, and fails later somewhere the
    /// cause is invisible. Refusing costs a bigger buffer; not refusing costs
    /// an evening.
    pub fn parse_read(raw: &[u8], capacity: usize) -> Result<Self, ConfigError> {
        if raw.len() >= capacity {
            return Err(ConfigError::Truncated);
        }
        let text = core::str::from_utf8(raw).map_err(|_| ConfigError::NotText)?;
        Self::parse(text)
    }

    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let mut ssid: Option<String<MAX_SSID>> = None;
        let mut password: String<MAX_PASSWORD> = String::new();
        let mut server: Option<String<MAX_SERVER>> = None;
        let mut insecure = false;

        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // Split on the first '=' only, so values may contain more.
            let Some((key, value)) = line.split_once('=') else {
                return Err(ConfigError::MalformedLine);
            };
            let key = key.trim();
            let value = value.trim();

            match key {
                "ssid" => {
                    let value = strip_comment(value);
                    ssid = Some(String::try_from(value).map_err(|_| ConfigError::ValueTooLong)?);
                }
                // Not comment-stripped, deliberately: see the module docs.
                "password" => {
                    password = String::try_from(value).map_err(|_| ConfigError::ValueTooLong)?;
                }
                "server" => {
                    let value = strip_comment(value);
                    server = Some(String::try_from(value).map_err(|_| ConfigError::ValueTooLong)?);
                }
                "insecure" => {
                    insecure = parse_bool(strip_comment(value))?;
                }
                // Unknown keys are ignored so a newer config file does not
                // brick an older firmware.
                _ => {}
            }
        }

        // A key present but empty is the same mistake as a key left out. An
        // empty password is not: an open network is a real thing.
        Ok(Config {
            ssid: ssid
                .filter(|s| !s.is_empty())
                .ok_or(ConfigError::MissingSsid)?,
            password,
            server: server
                .filter(|s| !s.is_empty())
                .ok_or(ConfigError::MissingServer)?,
            insecure,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key with nothing after the `=` is the same mistake as leaving the key
    /// out: there is no network called "". Accepting it turned a typo into a
    /// WiFi failure diagnosed at the box rather than at the config file.
    #[test]
    fn an_ssid_with_no_value_is_missing_rather_than_empty() {
        let err = Config::parse("ssid =\nserver = box.lan:8080\n").unwrap_err();
        assert_eq!(err, ConfigError::MissingSsid);
    }

    #[test]
    fn a_server_with_no_value_is_missing_rather_than_empty() {
        let err = Config::parse("ssid = home\nserver =   \n").unwrap_err();
        assert_eq!(err, ConfigError::MissingServer);
    }

    #[test]
    fn a_trailing_comment_is_not_part_of_the_server() {
        let c = Config::parse("ssid = home\nserver = box.lan:8080 # our box\n").unwrap();
        assert_eq!(c.server.as_str(), "box.lan:8080");
    }

    /// Only a `#` that starts a word is a comment, so a value may contain one.
    #[test]
    fn a_hash_inside_a_value_is_part_of_the_value() {
        let c = Config::parse("ssid = net#1\nserver = box.lan:8080\n").unwrap();
        assert_eq!(c.ssid.as_str(), "net#1");
    }

    /// The exception that the whole rule is shaped around: WiFi passwords
    /// contain `#` often, and truncating one fails at the box rather than in
    /// the file, where nobody can see why.
    #[test]
    fn a_password_keeps_a_hash_and_everything_after_it() {
        let c =
            Config::parse("ssid = home\npassword = hunter2 #1\nserver = box.lan:8080\n").unwrap();
        assert_eq!(c.password.as_str(), "hunter2 #1");
    }

    /// An open network is a real thing, so this one stays permitted.
    #[test]
    fn an_empty_password_is_allowed() {
        let c = Config::parse("ssid = home\npassword =\nserver = box.lan:8080\n").unwrap();
        assert_eq!(c.password.as_str(), "");
    }

    #[test]
    fn parses_a_minimal_file() {
        let c =
            Config::parse("ssid = HomeNet\npassword = hunter2\nserver = 10.0.0.5:8080\n").unwrap();
        assert_eq!(c.ssid.as_str(), "HomeNet");
        assert_eq!(c.password.as_str(), "hunter2");
        assert_eq!(c.server.as_str(), "10.0.0.5:8080");
    }

    #[test]
    fn ignores_comments_and_blank_lines() {
        let text = "# my box\n\nssid = HomeNet\n\n# the server\nserver = box.lan:8080\n";
        let c = Config::parse(text).unwrap();
        assert_eq!(c.ssid.as_str(), "HomeNet");
        assert_eq!(c.server.as_str(), "box.lan:8080");
    }

    #[test]
    fn tolerates_missing_and_extra_whitespace() {
        let c = Config::parse("ssid=HomeNet\n   server   =   box.lan:8080   \n").unwrap();
        assert_eq!(c.ssid.as_str(), "HomeNet");
        assert_eq!(c.server.as_str(), "box.lan:8080");
    }

    #[test]
    fn accepts_windows_line_endings() {
        let c = Config::parse("ssid = HomeNet\r\nserver = box.lan:8080\r\n").unwrap();
        assert_eq!(c.ssid.as_str(), "HomeNet");
        assert_eq!(c.server.as_str(), "box.lan:8080");
    }

    #[test]
    fn an_open_network_needs_no_password() {
        let c = Config::parse("ssid = Cafe\nserver = box.lan:8080\n").unwrap();
        assert!(c.password.is_empty());
    }

    #[test]
    fn a_password_may_contain_equals_signs() {
        let c = Config::parse("ssid = A\npassword = a=b=c\nserver = s:1\n").unwrap();
        assert_eq!(c.password.as_str(), "a=b=c");
    }

    #[test]
    fn a_missing_ssid_is_an_error() {
        assert_eq!(
            Config::parse("server = box.lan:8080\n"),
            Err(ConfigError::MissingSsid)
        );
    }

    #[test]
    fn a_missing_server_is_an_error() {
        assert_eq!(
            Config::parse("ssid = HomeNet\n"),
            Err(ConfigError::MissingServer)
        );
    }

    #[test]
    fn a_line_without_a_separator_is_an_error() {
        assert_eq!(
            Config::parse("ssid = A\nserver = s:1\nnonsense\n"),
            Err(ConfigError::MalformedLine)
        );
    }

    #[test]
    fn an_overlong_value_is_rejected_rather_than_truncated() {
        // 40 characters, over the 32-byte SSID limit.
        const TEXT: &str = "ssid = xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\nserver = s:1\n";
        assert_eq!(Config::parse(TEXT), Err(ConfigError::ValueTooLong));
    }

    /// The default has to be the safe one. A file that says nothing about
    /// certificates must not quietly get less checking than one that does.
    #[test]
    fn a_missing_insecure_key_leaves_certificate_checking_on() {
        let c = Config::parse("ssid = A\nserver = s:1\n").unwrap();
        assert!(!c.insecure);
    }

    #[test]
    fn insecure_yes_turns_certificate_checking_off() {
        let c = Config::parse("ssid = A\nserver = s:1\ninsecure = yes\n").unwrap();
        assert!(c.insecure);
    }

    #[test]
    fn insecure_no_leaves_certificate_checking_on() {
        let c = Config::parse("ssid = A\nserver = s:1\ninsecure = no\n").unwrap();
        assert!(!c.insecure);
    }

    /// `true`/`false` as well as `yes`/`no`, because both are what people type,
    /// and case is not a thing worth failing a box over.
    #[test]
    fn insecure_accepts_true_and_false_in_any_case() {
        assert!(
            Config::parse("ssid = A\nserver = s:1\ninsecure = TRUE\n")
                .unwrap()
                .insecure
        );
        assert!(
            !Config::parse("ssid = A\nserver = s:1\ninsecure = False\n")
                .unwrap()
                .insecure
        );
        assert!(
            Config::parse("ssid = A\nserver = s:1\ninsecure = Yes\n")
                .unwrap()
                .insecure
        );
    }

    /// The dangerous direction is a typo that reads as "off". Refusing the
    /// value outright means the parent is told, at the box, that the line did
    /// not do what they meant — rather than the box silently checking
    /// certificates they believed it was not, or not checking ones they
    /// believed it was.
    #[test]
    fn an_unrecognised_insecure_value_is_refused_rather_than_guessed() {
        assert_eq!(
            Config::parse("ssid = A\nserver = s:1\ninsecure = ture\n"),
            Err(ConfigError::MalformedValue)
        );
    }

    /// An empty value is the same mistake as a misspelt one, and it is the
    /// likelier typo: a line left half-written.
    #[test]
    fn an_empty_insecure_value_is_refused() {
        assert_eq!(
            Config::parse("ssid = A\nserver = s:1\ninsecure =\n"),
            Err(ConfigError::MalformedValue)
        );
    }

    /// It is comment-stripped, unlike `password`: there is no boolean that
    /// needs a `#` in it.
    #[test]
    fn a_trailing_comment_is_not_part_of_the_insecure_value() {
        let c = Config::parse("ssid = A\nserver = s:1\ninsecure = yes # bench only\n").unwrap();
        assert!(c.insecure);
    }
    /// A read that exactly filled the buffer is indistinguishable from one
    /// that ran out of room, so it is refused. The failure it prevents is the
    /// quiet one: a file cut mid-line still parses, and `server = teddycloud.l`
    /// is a plausible-looking wrong answer that fails much later and somewhere
    /// else.
    #[test]
    fn a_read_that_filled_the_buffer_is_refused_rather_than_parsed() {
        let raw = b"ssid = A\nserver = s:1\n";
        assert_eq!(
            Config::parse_read(raw, raw.len()),
            Err(ConfigError::Truncated)
        );
    }

    /// Room left over means the file ended on its own.
    #[test]
    fn a_read_with_room_to_spare_is_a_whole_file() {
        let raw = b"ssid = A\nserver = s:1\n";
        let c = Config::parse_read(raw, raw.len() + 1).unwrap();
        assert_eq!(c.ssid.as_str(), "A");
    }

    /// A card can hold anything. Bytes that are not text are not a config
    /// file, and saying so beats a parse error about a line nobody wrote.
    #[test]
    fn bytes_that_are_not_text_are_refused() {
        assert_eq!(
            Config::parse_read(&[0xFF, 0xFE, 0x00], 64),
            Err(ConfigError::NotText)
        );
    }
}
