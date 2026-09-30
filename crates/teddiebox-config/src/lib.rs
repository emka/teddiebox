#![no_std]

//! Parses the box's configuration file from the SD card.
//!
//! The format is kept as simple as possible, so a parent editing it on a
//! laptop cannot easily get it wrong: `key = value`, one per line, `#`
//! comments, blank lines ignored.
//!
//! A `#` at the start of a word starts a comment, so
//! `server = box.lan:8080 # ours` works, while `ssid = net#1` keeps its `#`.
//! **`password` is an exception**: passwords often contain `#`, and a cut-off
//! password would fail with no visible cause. Everything after `password =`
//! is the password. The same applies to `setup_password`.
//!
//! Unknown keys are ignored, so a card written for newer firmware still works
//! with older firmware. A *known* key with a value it does not accept is an
//! error: `ears_skip = ture` is a typo, and reporting it is better than
//! guessing.

use heapless::String;

/// The file's name in the card's root directory.
///
/// A **short** (8.3) name, so `embedded-sdmmc` can open it with the ordinary
/// `open_file_in_dir`, which only accepts short names.
///
/// Upper case because that is how FAT stores 8.3 names. A lower-case
/// `config.txt` on the card is the same directory entry (with "display lower
/// case" flags set) and opens with this name. `tools/fat-assumptions` tests
/// this against the real library.
///
/// A long name would need `open_long_name_file_in_dir`, which is slower,
/// cannot create files, and would add a second way into the filesystem.
///
/// `.TXT` so a laptop opens it in a text editor.
pub const FILENAME: &str = "CONFIG.TXT";

pub const MAX_SSID: usize = 32;
pub const MAX_PASSWORD: usize = 63;
pub const MAX_SERVER: usize = 64;
/// The example on [`Config::update_url`] is 55 characters; 128 leaves room for
/// a longer path.
pub const MAX_UPDATE_URL: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub ssid: String<MAX_SSID>,
    pub password: String<MAX_PASSWORD>,
    /// `host:port` of the teddyCloud server.
    pub server: String<MAX_SERVER>,
    /// Whether holding an ear skips a chapter.
    ///
    /// A stock box's ears only change the volume; skipping is this firmware's
    /// addition, so it is a setting on the card rather than a rebuild.
    ///
    /// Defaults to `true`. When off, a held ear changes the volume like any
    /// other press, as on a stock box.
    pub ears_skip: bool,
    /// Where to fetch the OTA manifest, in full — scheme, host, port and
    /// path, e.g.
    /// `https://teddycloud.local:8443/content/FIRMWARE/teddiebox.txt`.
    ///
    /// **Optional. `None` (the default) turns OTA off**: the box does not
    /// check for updates. There is no safe default location, and an old card
    /// must not start fetching firmware.
    pub update_url: Option<String<MAX_UPDATE_URL>>,
    /// Passphrase for the box's own setup access point.
    ///
    /// `None` means the built-in passphrase, which is published in
    /// `README.md`: the way back into a broken box cannot depend on a file
    /// that might be wrong. Setting this limits the ten-minute setup window to
    /// people who know it. A forgotten one is reset over the serial console
    /// in setup mode (`setup pw off`), or removed from the card in a reader.
    pub setup_password: Option<String<MAX_PASSWORD>>,
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
    /// `update_url` was given but has no content.
    ///
    /// Unlike `ssid` and `server`, a missing `update_url` is not an error (it
    /// turns OTA off). But `update_url =` with nothing after it is probably a
    /// mistake, so it gets its own error.
    EmptyUpdateUrl,
}

/// Writes `text` into `out` with one key changed, added or taken out.
///
/// Every other line is kept exactly as it was (comments, blank lines,
/// spacing, order), because a person wrote this file and will read it again.
///
/// A `value` of `None` removes the key's line; removing a key that is not
/// there is not an error. Setting a key that is not there appends it on its
/// own line.
///
/// Returns an error instead of truncating when `out` is too small: a config
/// cut off mid-line is dangerous. See [`Config::parse_read`].
pub fn set_key<const N: usize>(
    text: &str,
    key: &str,
    value: Option<&str>,
    out: &mut String<N>,
) -> Result<(), ConfigError> {
    let too_long = |_| ConfigError::ValueTooLong;
    let mut replaced = false;

    for line in text.lines() {
        let names_the_key = line
            .split_once('=')
            .is_some_and(|(name, _)| name.trim() == key);
        if names_the_key {
            replaced = true;
            let Some(value) = value else {
                continue;
            };
            out.push_str(key).map_err(too_long)?;
            out.push_str(" = ").map_err(too_long)?;
            out.push_str(value).map_err(too_long)?;
        } else {
            out.push_str(line).map_err(too_long)?;
        }
        out.push('\n').map_err(too_long)?;
    }

    if !replaced {
        if let Some(value) = value {
            out.push_str(key).map_err(too_long)?;
            out.push_str(" = ").map_err(too_long)?;
            out.push_str(value).map_err(too_long)?;
            out.push('\n').map_err(too_long)?;
        }
    }

    Ok(())
}

/// The range WPA2 accepts for a passphrase.
///
/// Outside this range the driver cannot start the setup access point, and
/// then nobody can reach the box to find out why. So the parser rejects it.
const WPA2_PASSPHRASE: core::ops::RangeInclusive<usize> = 8..=63;

/// Says whether a value is shaped like the `host:port` a `Host:` header wants.
///
/// The value is copied into a request header without escaping, and lines are
/// split on `\n` only, so a `CR` inside the value would end the header line
/// early, and a space would split it. Only characters found in hostnames,
/// addresses and ports are allowed.
///
/// A bad value is rejected, not repaired: it is a typo.
///
/// An empty value passes, so that `server =` still reports `MissingServer`.
fn is_host_port(value: &str) -> bool {
    value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':'))
}

/// Why [`split_host_port`] could not split a `host:port` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitHostPortError {
    /// There is no `:` to split on.
    NoPort,
    /// What follows the last `:` is not a `u16`.
    BadPort,
    /// The host is empty.
    EmptyHost,
    /// The host is longer than the caller's buffer.
    HostTooLong,
}

/// Splits `host:port`, the way `server` is written in `CONFIG.TXT`, and
/// checks the host fits in `max_host_len` bytes.
///
/// Splits on the **last** `:`, so a value that is already only characters
/// [`is_host_port`] accepts never needs a bracketed IPv6 form: nothing here
/// has ever carried one.
pub fn split_host_port(
    value: &str,
    max_host_len: usize,
) -> Result<(&str, u16), SplitHostPortError> {
    let (host, port) = value.rsplit_once(':').ok_or(SplitHostPortError::NoPort)?;
    let port: u16 = port.parse().map_err(|_| SplitHostPortError::BadPort)?;
    if host.is_empty() {
        return Err(SplitHostPortError::EmptyHost);
    }
    if host.len() > max_host_len {
        return Err(SplitHostPortError::HostTooLong);
    }
    Ok((host, port))
}

/// The box's current configuration: what the card says, plus anything typed
/// at the console.
///
/// Every change goes through these methods, which record that a value was
/// typed, so a later [`take_card`](Self::take_card) does not overwrite it.
/// See [`Overridden`].
///
/// The caller is responsible for locking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    held: Config,
    overridden: Overridden,
}

impl Settings {
    /// An empty configuration: no credentials, no server, ears skipping
    /// chapters.
    ///
    /// `const` so it can start a `static` without a lazy initialiser.
    pub const fn new() -> Self {
        Self {
            held: Config {
                ssid: String::new(),
                password: String::new(),
                server: String::new(),
                ears_skip: true,
                update_url: None,
                setup_password: None,
            },
            overridden: Overridden {
                ssid: false,
                password: false,
                server: false,
                ears_skip: false,
                update_url: false,
                setup_password: false,
            },
        }
    }

    /// The current configuration, with console overrides applied.
    pub fn config(&self) -> &Config {
        &self.held
    }

    /// Takes the card's value for every setting that was not typed.
    ///
    /// The media task calls this when it first mounts the card, which can be
    /// *after* something was typed: the card is mounted on first use, often by
    /// a network command.
    pub fn take_card(&mut self, card: Config) {
        self.held = self.overridden.merge(card, &self.held);
    }

    pub fn set_ssid(&mut self, value: String<MAX_SSID>) {
        self.held.ssid = value;
        self.overridden.ssid = true;
    }

    pub fn set_password(&mut self, value: String<MAX_PASSWORD>) {
        self.held.password = value;
        self.overridden.password = true;
    }

    pub fn set_ears_skip(&mut self, value: bool) {
        self.held.ears_skip = value;
        self.overridden.ears_skip = true;
    }

    /// The whole configuration, or `None` if it is not yet usable for joining
    /// a network.
    ///
    /// Needs both SSID and passphrase. An empty one would make joining fail
    /// as if the passphrase were wrong.
    pub fn credentials(&self) -> Option<&Config> {
        if self.held.ssid.is_empty() || self.held.password.is_empty() {
            return None;
        }
        Some(&self.held)
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self::new()
    }
}

/// Cuts a trailing `# comment` off a value.
///
/// The `#` must start a word (be first, or follow whitespace), so `net#1` is
/// kept while `net #1` is cut. Not used for `password`.
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
/// Accepts `yes`/`no`, `true`/`false` (any case) and `1`/`0`. Anything else is
/// an error, not a default: the line did not say what was meant.
fn parse_bool(value: &str) -> Result<bool, ConfigError> {
    // `eq_ignore_ascii_case` compares in place; there is no allocator here.
    if value.eq_ignore_ascii_case("yes") || value.eq_ignore_ascii_case("true") || value == "1" {
        Ok(true)
    } else if value.eq_ignore_ascii_case("no")
        || value.eq_ignore_ascii_case("false")
        || value == "0"
    {
        Ok(false)
    } else {
        Err(ConfigError::MalformedValue)
    }
}

/// Which settings were typed at the console, so a later card read does not
/// undo them.
///
/// The card is read when it is first mounted, which can happen *after*
/// something was typed (the first user of the card is often a network
/// command). Replacing the whole config at that point would lose the typed
/// values.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Overridden {
    pub ssid: bool,
    pub password: bool,
    pub server: bool,
    pub ears_skip: bool,
    pub update_url: bool,
    pub setup_password: bool,
}

impl Overridden {
    /// Takes the card's value for every setting that was *not* typed.
    ///
    /// Per field: typing a passphrase must not keep an old SSID from before
    /// the card was read.
    pub fn merge(self, card: Config, held: &Config) -> Config {
        Config {
            ssid: if self.ssid {
                held.ssid.clone()
            } else {
                card.ssid
            },
            password: if self.password {
                held.password.clone()
            } else {
                card.password
            },
            server: if self.server {
                held.server.clone()
            } else {
                card.server
            },
            ears_skip: if self.ears_skip {
                held.ears_skip
            } else {
                card.ears_skip
            },
            update_url: if self.update_url {
                held.update_url.clone()
            } else {
                card.update_url
            },
            setup_password: if self.setup_password {
                held.setup_password.clone()
            } else {
                card.setup_password
            },
        }
    }
}

impl Config {
    /// Parses a file that was read into a fixed buffer.
    ///
    /// `capacity` is the size of that buffer. **A read that filled it exactly
    /// is refused**, because a file that just fits cannot be told apart from
    /// one that was cut off. A cut-off file can still parse, into a wrong
    /// value that fails later with no visible cause.
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
        let mut ears_skip = true;
        let mut update_url: Option<String<MAX_UPDATE_URL>> = None;
        let mut setup_password: Option<String<MAX_PASSWORD>> = None;

        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // Split on the first '=' only, so values may contain more.
            let Some((key, value)) = line.split_once('=') else {
                return Err(ConfigError::MalformedLine);
            };
            apply_line(
                key.trim(),
                value.trim(),
                &mut ssid,
                &mut password,
                &mut server,
                &mut ears_skip,
                &mut update_url,
                &mut setup_password,
            )?;
        }

        // An empty key counts as missing. An empty password is allowed, for
        // open networks.
        Ok(Config {
            ssid: ssid
                .filter(|s| !s.is_empty())
                .ok_or(ConfigError::MissingSsid)?,
            password,
            server: server
                .filter(|s| !s.is_empty())
                .ok_or(ConfigError::MissingServer)?,
            ears_skip,
            update_url,
            setup_password,
        })
    }
}

/// Applies one `key=value` line to the fields being built.
///
/// Unknown keys are ignored, so a newer config file works with older
/// firmware.
#[allow(clippy::too_many_arguments)]
fn apply_line(
    key: &str,
    value: &str,
    ssid: &mut Option<String<MAX_SSID>>,
    password: &mut String<MAX_PASSWORD>,
    server: &mut Option<String<MAX_SERVER>>,
    ears_skip: &mut bool,
    update_url: &mut Option<String<MAX_UPDATE_URL>>,
    setup_password: &mut Option<String<MAX_PASSWORD>>,
) -> Result<(), ConfigError> {
    match key {
        "ssid" => {
            let value = strip_comment(value);
            *ssid = Some(String::try_from(value).map_err(|_| ConfigError::ValueTooLong)?);
        }
        // No comment stripping: see the module docs.
        "password" => {
            *password = String::try_from(value).map_err(|_| ConfigError::ValueTooLong)?;
        }
        "server" => {
            let value = strip_comment(value);
            if !is_host_port(value) {
                return Err(ConfigError::MalformedValue);
            }
            *server = Some(String::try_from(value).map_err(|_| ConfigError::ValueTooLong)?);
        }
        "ears_skip" => {
            *ears_skip = parse_bool(strip_comment(value))?;
        }
        // No comment stripping, like `password`.
        "setup_password" => {
            if !WPA2_PASSPHRASE.contains(&value.len()) {
                return Err(ConfigError::MalformedValue);
            }
            *setup_password = Some(String::try_from(value).map_err(|_| ConfigError::ValueTooLong)?);
        }
        "update_url" => {
            let value = strip_comment(value);
            if value.is_empty() {
                return Err(ConfigError::EmptyUpdateUrl);
            }
            *update_url = Some(String::try_from(value).map_err(|_| ConfigError::ValueTooLong)?);
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::format;

    use super::*;

    /// A key with nothing after the `=` counts as missing: there is no
    /// network called "".
    #[test]
    fn an_ssid_with_no_value_is_missing_rather_than_empty() {
        // Given
        let text = "ssid =\nserver = box.lan:8080\n";

        // When
        let err = Config::parse(text).unwrap_err();

        // Then
        assert_eq!(err, ConfigError::MissingSsid);
    }

    #[test]
    fn a_server_with_no_value_is_missing_rather_than_empty() {
        // Given
        let text = "ssid = home\nserver =   \n";

        // When
        let err = Config::parse(text).unwrap_err();

        // Then
        assert_eq!(err, ConfigError::MissingServer);
    }

    #[test]
    fn a_trailing_comment_is_not_part_of_the_server() {
        // Given
        let text = "ssid = home\nserver = box.lan:8080 # our box\n";

        // When
        let c = Config::parse(text).unwrap();

        // Then
        assert_eq!(c.server.as_str(), "box.lan:8080");
    }

    /// Only a `#` that starts a word is a comment, so a value may contain one.
    #[test]
    fn a_hash_inside_a_value_is_part_of_the_value() {
        // Given
        let text = "ssid = net#1\nserver = box.lan:8080\n";

        // When
        let c = Config::parse(text).unwrap();

        // Then
        assert_eq!(c.ssid.as_str(), "net#1");
    }

    /// Wi-Fi passwords often contain `#`, so the password keeps everything.
    #[test]
    fn a_password_keeps_a_hash_and_everything_after_it() {
        // Given
        let text = "ssid = home\npassword = hunter2 #1\nserver = box.lan:8080\n";

        // When
        let c = Config::parse(text).unwrap();

        // Then
        assert_eq!(c.password.as_str(), "hunter2 #1");
    }

    /// Allowed, for open networks.
    #[test]
    fn an_empty_password_is_allowed() {
        // Given
        let text = "ssid = home\npassword =\nserver = box.lan:8080\n";

        // When
        let c = Config::parse(text).unwrap();

        // Then
        assert_eq!(c.password.as_str(), "");
    }

    #[test]
    fn parses_a_minimal_file() {
        // Given
        let text = "ssid = HomeNet\npassword = hunter2\nserver = 10.0.0.5:8080\n";

        // When
        let c = Config::parse(text).unwrap();

        // Then
        assert_eq!(c.ssid.as_str(), "HomeNet");
        assert_eq!(c.password.as_str(), "hunter2");
        assert_eq!(c.server.as_str(), "10.0.0.5:8080");
    }

    #[test]
    fn ignores_comments_and_blank_lines() {
        // Given
        let text = "# my box\n\nssid = HomeNet\n\n# the server\nserver = box.lan:8080\n";

        // When
        let c = Config::parse(text).unwrap();

        // Then
        assert_eq!(c.ssid.as_str(), "HomeNet");
        assert_eq!(c.server.as_str(), "box.lan:8080");
    }

    #[test]
    fn tolerates_missing_and_extra_whitespace() {
        // Given
        let text = "ssid=HomeNet\n   server   =   box.lan:8080   \n";

        // When
        let c = Config::parse(text).unwrap();

        // Then
        assert_eq!(c.ssid.as_str(), "HomeNet");
        assert_eq!(c.server.as_str(), "box.lan:8080");
    }

    #[test]
    fn accepts_windows_line_endings() {
        // Given
        let text = "ssid = HomeNet\r\nserver = box.lan:8080\r\n";

        // When
        let c = Config::parse(text).unwrap();

        // Then
        assert_eq!(c.ssid.as_str(), "HomeNet");
        assert_eq!(c.server.as_str(), "box.lan:8080");
    }

    #[test]
    fn an_open_network_needs_no_password() {
        // Given
        let text = "ssid = Cafe\nserver = box.lan:8080\n";

        // When
        let c = Config::parse(text).unwrap();

        // Then
        assert!(c.password.is_empty());
    }

    #[test]
    fn a_password_may_contain_equals_signs() {
        // Given
        let text = "ssid = A\npassword = a=b=c\nserver = s:1\n";

        // When
        let c = Config::parse(text).unwrap();

        // Then
        assert_eq!(c.password.as_str(), "a=b=c");
    }

    #[test]
    fn a_missing_ssid_is_an_error() {
        // Given
        let text = "server = box.lan:8080\n";

        // When
        let parsed = Config::parse(text);

        // Then
        assert_eq!(parsed, Err(ConfigError::MissingSsid));
    }

    /// `server` is copied unescaped into a `Host:` header, and lines are split
    /// on `\n` only, so a `CR` inside the value would end the header line.
    #[test]
    fn a_server_carrying_a_bare_cr_is_refused() {
        // Given
        let text = "ssid = A\nserver = box.lan:8080\rX-Thing: 1\n";

        // When
        let parsed = Config::parse(text);

        // Then
        assert_eq!(parsed, Err(ConfigError::MalformedValue));
    }

    #[test]
    fn a_missing_server_is_an_error() {
        // Given
        let text = "ssid = HomeNet\n";

        // When
        let parsed = Config::parse(text);

        // Then
        assert_eq!(parsed, Err(ConfigError::MissingServer));
    }

    #[test]
    fn a_line_without_a_separator_is_an_error() {
        // Given
        let text = "ssid = A\nserver = s:1\nnonsense\n";

        // When
        let parsed = Config::parse(text);

        // Then
        assert_eq!(parsed, Err(ConfigError::MalformedLine));
    }

    #[test]
    fn an_overlong_value_is_rejected_rather_than_truncated() {
        // Given
        // 40 characters, over the 32-byte SSID limit.
        const TEXT: &str = "ssid = xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\nserver = s:1\n";

        // When
        let parsed = Config::parse(TEXT);

        // Then
        assert_eq!(parsed, Err(ConfigError::ValueTooLong));
    }

    /// Skipping is on unless the card turns it off.
    #[test]
    fn a_missing_ears_skip_key_leaves_the_ears_skipping() {
        // Given
        let text = "ssid = A\nserver = s:1\n";

        // When
        let c = Config::parse(text).unwrap();

        // Then
        assert!(c.ears_skip);
    }

    #[test]
    fn ears_skip_no_makes_the_ears_volume_only() {
        // Given
        let text = "ssid = A\nserver = s:1\nears_skip = no\n";

        // When
        let c = Config::parse(text).unwrap();

        // Then
        assert!(!c.ears_skip);
    }

    /// Each accepted spelling is written out, so the test can disagree with
    /// the parser.
    #[test]
    fn ears_skip_accepts_the_spellings_a_parent_might_reach_for() {
        // Given
        let spellings = ["yes", "true", "TRUE", "1", "no", "false", "False", "0"];

        // When
        let read = spellings.map(|value| {
            let text = format!("ssid = A\nserver = s:1\nears_skip = {value}\n");
            (value, Config::parse(&text).unwrap().ears_skip)
        });

        // Then
        assert_eq!(
            read,
            [
                ("yes", true),
                ("true", true),
                ("TRUE", true),
                ("1", true),
                ("no", false),
                ("false", false),
                ("False", false),
                ("0", false),
            ]
        );
    }

    /// A typo is an error, not silently `false`.
    #[test]
    fn a_misspelled_ears_skip_value_is_refused() {
        // Given
        let text = "ssid = A\nserver = s:1\nears_skip = yse\n";

        // When
        let parsed = Config::parse(text);

        // Then
        assert_eq!(parsed, Err(ConfigError::MalformedValue));
    }

    /// An empty value is an error too.
    #[test]
    fn an_empty_ears_skip_value_is_refused() {
        // Given
        let text = "ssid = A\nserver = s:1\nears_skip =\n";

        // When
        let parsed = Config::parse(text);

        // Then
        assert_eq!(parsed, Err(ConfigError::MalformedValue));
    }

    /// Unlike `password`, booleans have comments stripped.
    #[test]
    fn a_trailing_comment_is_not_part_of_a_boolean_value() {
        // Given
        let text = "ssid = A\nserver = s:1\nears_skip = no # volume only\n";

        // When
        let c = Config::parse(text).unwrap();

        // Then
        assert!(!c.ears_skip);
    }

    /// A read that exactly filled the buffer may have been cut off, so it is
    /// refused. A cut-off `server = teddycloud.l` would still parse.
    #[test]
    fn a_read_that_filled_the_buffer_is_refused_rather_than_parsed() {
        // Given
        let raw = b"ssid = A\nserver = s:1\n";

        // When
        let parsed = Config::parse_read(raw, raw.len());

        // Then
        assert_eq!(parsed, Err(ConfigError::Truncated));
    }

    /// Room left over means the file ended on its own.
    #[test]
    fn a_read_with_room_to_spare_is_a_whole_file() {
        // Given
        let raw = b"ssid = A\nserver = s:1\n";

        // When
        let c = Config::parse_read(raw, raw.len() + 1).unwrap();

        // Then
        assert_eq!(c.ssid.as_str(), "A");
    }

    /// Bytes that are not UTF-8 text are not a config file.
    #[test]
    fn bytes_that_are_not_text_are_refused() {
        // Given

        // When
        let parsed = Config::parse_read(&[0xFF, 0xFE, 0x00], 64);

        // Then
        assert_eq!(parsed, Err(ConfigError::NotText));
    }
    fn card() -> Config {
        Config::parse("ssid = FromCard\npassword = cardpw\nserver = card:1\nears_skip = no\n")
            .unwrap()
    }

    /// The card can be read *after* something was typed at the console; the
    /// typed value must survive.
    #[test]
    fn a_later_card_read_does_not_undo_what_the_bench_set() {
        // Given: skipping switched on at the bench, over a card that turns it
        // off
        let mut held = card();
        held.ears_skip = true;
        let overridden = Overridden {
            ears_skip: true,
            ..Overridden::default()
        };

        // When
        let merged = overridden.merge(card(), &held);

        // Then
        assert!(merged.ears_skip, "the card undid the override");
        assert_eq!(
            merged.ssid.as_str(),
            "FromCard",
            "and the rest still came from the card"
        );
    }

    /// With nothing typed, the card's values are used.
    #[test]
    fn an_untouched_setting_is_taken_from_the_card() {
        // Given
        let held = Config::parse("ssid = Old\nserver = old:1\n").unwrap();

        // When
        let merged = Overridden::default().merge(card(), &held);

        // Then
        assert_eq!(merged.ssid.as_str(), "FromCard");
        assert_eq!(merged.server.as_str(), "card:1");
        assert!(!merged.ears_skip);
    }

    /// Each field is independent: typing the passphrase must not keep an old
    /// SSID from before the card was read.
    #[test]
    fn overrides_are_per_field() {
        // Given: only the passphrase typed
        let mut held = card();
        held.password = heapless::String::try_from("typed").unwrap();
        let overridden = Overridden {
            password: true,
            ..Overridden::default()
        };

        // When
        let merged = overridden.merge(card(), &held);

        // Then
        assert_eq!(merged.password.as_str(), "typed");
        assert_eq!(merged.ssid.as_str(), "FromCard");
        assert_eq!(merged.server.as_str(), "card:1");
    }

    #[test]
    fn update_url_parses_when_present() {
        // Given
        let text = "ssid = A\nserver = s:1\nupdate_url = https://teddycloud.local:8443/content/FIRMWARE/teddiebox.txt\n";

        // When
        let c = Config::parse(text).unwrap();

        // Then
        assert_eq!(
            c.update_url.as_deref(),
            Some("https://teddycloud.local:8443/content/FIRMWARE/teddiebox.txt")
        );
    }

    /// A missing key turns OTA off; there is no default location.
    #[test]
    fn a_missing_update_url_key_leaves_it_none() {
        // Given
        let text = "ssid = A\nserver = s:1\n";

        // When
        let c = Config::parse(text).unwrap();

        // Then
        assert_eq!(c.update_url, None);
    }

    #[test]
    fn an_overlong_update_url_is_rejected_rather_than_truncated() {
        // Given
        // 143 characters, over the 128-byte limit.
        const TEXT: &str = "ssid = A\nserver = s:1\nupdate_url = https://example.com/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n";

        // When
        let parsed = Config::parse(TEXT);

        // Then
        assert_eq!(parsed, Err(ConfigError::ValueTooLong));
    }

    /// Tests the exact limit, so a change to `MAX_UPDATE_URL` is caught.
    #[test]
    fn an_update_url_at_exactly_the_limit_is_accepted() {
        // Given
        let value = format!("https://example.com/{}", "x".repeat(MAX_UPDATE_URL - 20));
        assert_eq!(value.len(), MAX_UPDATE_URL);
        let text = format!("ssid = A\nserver = s:1\nupdate_url = {value}\n");

        // When
        let c = Config::parse(&text).unwrap();

        // Then
        assert_eq!(c.update_url.as_deref(), Some(value.as_str()));
    }

    #[test]
    fn an_update_url_one_byte_over_the_limit_is_refused() {
        // Given
        let value = format!("https://example.com/{}", "x".repeat(MAX_UPDATE_URL - 19));
        assert_eq!(value.len(), MAX_UPDATE_URL + 1);
        let text = format!("ssid = A\nserver = s:1\nupdate_url = {value}\n");

        // When
        let parsed = Config::parse(&text);

        // Then
        assert_eq!(parsed, Err(ConfigError::ValueTooLong));
    }

    /// `update_url =` with nothing after it is probably a mistake, not a way
    /// to turn OTA off.
    #[test]
    fn an_empty_update_url_is_refused() {
        // Given
        let text = "ssid = A\nserver = s:1\nupdate_url =\n";

        // When
        let parsed = Config::parse(text);

        // Then
        assert_eq!(parsed, Err(ConfigError::EmptyUpdateUrl));
    }

    #[test]
    fn a_whitespace_only_update_url_is_refused() {
        // Given
        let text = "ssid = A\nserver = s:1\nupdate_url =    \n";

        // When
        let parsed = Config::parse(text);

        // Then
        assert_eq!(parsed, Err(ConfigError::EmptyUpdateUrl));
    }

    #[test]
    fn a_trailing_comment_is_not_part_of_the_update_url() {
        // Given
        let text =
            "ssid = A\nserver = s:1\nupdate_url = https://teddycloud.local/teddiebox.txt # ours\n";

        // When
        let c = Config::parse(text).unwrap();

        // Then
        assert_eq!(
            c.update_url.as_deref(),
            Some("https://teddycloud.local/teddiebox.txt")
        );
    }

    /// Only a `#` that starts a word is a comment, the same rule as for
    /// `ssid` and `server`.
    #[test]
    fn a_hash_inside_the_update_url_is_part_of_the_value() {
        // Given
        let text = "ssid = A\nserver = s:1\nupdate_url = https://x/y#z\n";

        // When
        let c = Config::parse(text).unwrap();

        // Then
        assert_eq!(c.update_url.as_deref(), Some("https://x/y#z"));
    }

    /// Something is typed at the console, then the card is mounted. The typed
    /// value wins, and every other value comes from the card.
    #[test]
    fn a_typed_value_wins_over_a_card_read_after_it() {
        // Given
        let mut settings = Settings::new();
        settings.set_ssid(String::try_from("Typed").unwrap());
        settings.set_ears_skip(true);

        // When
        settings.take_card(card());

        // Then
        assert_eq!(settings.config().ssid.as_str(), "Typed");
        assert!(settings.config().ears_skip);
        assert_eq!(settings.config().server.as_str(), "card:1");
    }

    /// The other order: the card is read first, then a typed value overrides
    /// it.
    #[test]
    fn a_value_typed_after_a_card_read_still_wins() {
        // Given
        let mut settings = Settings::new();
        settings.take_card(card());

        // When
        settings.set_ssid(String::try_from("Typed").unwrap());

        // Then
        assert_eq!(settings.config().ssid.as_str(), "Typed");
        assert_eq!(settings.config().server.as_str(), "card:1");
    }

    /// Credentials need both SSID and passphrase.
    #[test]
    fn credentials_are_withheld_until_both_halves_are_there() {
        // Given
        let mut settings = Settings::new();

        // When: nothing typed, then an SSID, then a passphrase
        let with_nothing = settings.credentials().is_some();
        settings.set_ssid(String::try_from("Typed").unwrap());
        let with_an_ssid = settings.credentials().is_some();
        settings.set_password(String::try_from("hunter2").unwrap());
        let with_both = settings.credentials().is_some();

        // Then
        assert_eq!(
            (with_nothing, with_an_ssid, with_both),
            (false, false, true)
        );
    }

    #[test]
    fn a_setup_password_is_read_when_the_card_gives_one() {
        // Given
        let text = "ssid = A\nserver = s:1\nsetup_password = our#house\n";

        // When
        let c = Config::parse(text).unwrap();

        // Then
        assert_eq!(c.setup_password.as_deref(), Some("our#house"));
    }

    /// No `setup_password` means the built-in one is used.
    #[test]
    fn no_setup_password_is_absent_rather_than_empty() {
        // Given
        let text = "ssid = A\nserver = s:1\n";

        // When
        let c = Config::parse(text).unwrap();

        // Then
        assert_eq!(c.setup_password, None);
    }

    /// WPA2 needs 8 to 63 characters. Otherwise the setup access point could
    /// not start, so the parser rejects it.
    #[test]
    fn a_setup_password_too_short_for_wpa2_is_refused() {
        // Given
        let text = "ssid = A\nserver = s:1\nsetup_password = short\n";

        // When
        let parsed = Config::parse(text);

        // Then
        assert_eq!(parsed, Err(ConfigError::MalformedValue));
    }

    #[test]
    fn a_setup_password_too_long_for_wpa2_is_refused() {
        // Given
        let long = "x".repeat(64);

        // When
        let parsed = Config::parse(&format!(
            "ssid = A\nserver = s:1\nsetup_password = {long}\n"
        ));

        // Then
        assert_eq!(parsed, Err(ConfigError::MalformedValue));
    }

    #[test]
    fn rewriting_a_key_leaves_every_other_line_exactly_as_it_was() {
        // Given
        let mut out = String::<256>::new();

        // When
        set_key(
            "# our box\n\nssid = Home\nsetup_password = old one\nserver = s:1\n",
            "setup_password",
            Some("a new one"),
            &mut out,
        )
        .unwrap();

        // Then
        assert_eq!(
            out.as_str(),
            "# our box\n\nssid = Home\nsetup_password = a new one\nserver = s:1\n"
        );
    }

    #[test]
    fn removing_a_key_takes_its_whole_line_with_it() {
        // Given
        let mut out = String::<256>::new();

        // When
        set_key(
            "ssid = Home\nsetup_password = old one\nserver = s:1\n",
            "setup_password",
            None,
            &mut out,
        )
        .unwrap();

        // Then
        assert_eq!(out.as_str(), "ssid = Home\nserver = s:1\n");
    }

    #[test]
    fn a_key_that_is_not_there_yet_is_appended() {
        // Given
        let mut out = String::<256>::new();

        // When
        set_key(
            "ssid = Home\n",
            "setup_password",
            Some("a new one"),
            &mut out,
        )
        .unwrap();

        // Then
        assert_eq!(out.as_str(), "ssid = Home\nsetup_password = a new one\n");
    }

    /// What `setup pw off` does on a card that never had the key.
    #[test]
    fn removing_a_key_that_is_not_there_changes_nothing() {
        // Given
        let mut out = String::<256>::new();

        // When
        set_key("ssid = Home\n", "setup_password", None, &mut out).unwrap();

        // Then
        assert_eq!(out.as_str(), "ssid = Home\n");
    }

    /// Appending to a file without a final newline must not join two keys
    /// on one line.
    #[test]
    fn appending_to_a_file_with_no_trailing_newline_still_starts_a_line() {
        // Given
        let mut out = String::<256>::new();

        // When
        set_key("ssid = Home", "setup_password", Some("a new one"), &mut out).unwrap();

        // Then
        assert_eq!(out.as_str(), "ssid = Home\nsetup_password = a new one\n");
    }

    #[test]
    fn a_result_too_long_for_the_buffer_is_refused_rather_than_truncated() {
        // Given
        let mut out = String::<16>::new();

        // When
        let result = set_key(
            "ssid = Home\n",
            "setup_password",
            Some("a new one"),
            &mut out,
        );

        // Then
        assert_eq!(result, Err(ConfigError::ValueTooLong));
    }

    /// A typed `update_url` survives a later card read.
    #[test]
    fn overridden_update_url_survives_a_later_card_read() {
        // Given
        let mut held = card();
        held.update_url = Some(heapless::String::try_from("https://typed/teddiebox.txt").unwrap());
        let overridden = Overridden {
            update_url: true,
            ..Overridden::default()
        };

        // When
        let merged = overridden.merge(card(), &held);

        // Then
        assert_eq!(
            merged.update_url.as_deref(),
            Some("https://typed/teddiebox.txt")
        );
        assert_eq!(merged.ssid.as_str(), "FromCard");
    }

    #[test]
    fn an_untouched_update_url_is_taken_from_the_card() {
        // Given
        let mut typed_card = card();
        typed_card.update_url =
            Some(heapless::String::try_from("https://card/teddiebox.txt").unwrap());
        let held = Config::parse("ssid = Old\nserver = old:1\n").unwrap();

        // When
        let merged = Overridden::default().merge(typed_card, &held);

        // Then
        assert_eq!(
            merged.update_url.as_deref(),
            Some("https://card/teddiebox.txt")
        );
    }

    #[test]
    fn a_host_and_port_split_on_the_last_colon() {
        // Given
        let value = "teddycloud.local:443";

        // When
        let split = split_host_port(value, 80);

        // Then
        assert_eq!(split, Ok(("teddycloud.local", 443)));
    }

    #[test]
    fn a_value_with_no_colon_has_no_port() {
        // Given
        let value = "teddycloud.local";

        // When
        let split = split_host_port(value, 80);

        // Then
        assert_eq!(split, Err(SplitHostPortError::NoPort));
    }

    #[test]
    fn a_port_that_is_not_a_number_is_refused() {
        // Given
        let value = "teddycloud.local:https";

        // When
        let split = split_host_port(value, 80);

        // Then
        assert_eq!(split, Err(SplitHostPortError::BadPort));
    }

    #[test]
    fn a_port_above_u16_is_refused() {
        // Given
        let value = "teddycloud.local:65536";

        // When
        let split = split_host_port(value, 80);

        // Then
        assert_eq!(split, Err(SplitHostPortError::BadPort));
    }

    #[test]
    fn an_empty_host_is_refused() {
        // Given
        let value = ":443";

        // When
        let split = split_host_port(value, 80);

        // Then
        assert_eq!(split, Err(SplitHostPortError::EmptyHost));
    }

    #[test]
    fn a_host_longer_than_the_buffer_is_refused_rather_than_truncated() {
        // Given
        let value = "teddycloud.local:443";

        // When
        let split = split_host_port(value, 5);

        // Then
        assert_eq!(split, Err(SplitHostPortError::HostTooLong));
    }

    #[test]
    fn a_host_exactly_at_the_limit_is_accepted() {
        // Given
        let value = "teddycloud.local:443";

        // When
        let split = split_host_port(value, "teddycloud.local".len());

        // Then
        assert_eq!(split, Ok(("teddycloud.local", 443)));
    }
}
