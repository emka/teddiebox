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
/// **Short**, so that `embedded-sdmmc` opens it with the ordinary
/// `open_file_in_dir` — which takes a `ShortFileName` and refuses the ninth
/// character of a stem.
///
/// Upper case here because that is how FAT *stores* an 8.3 name, not because
/// the card has to show it that way: a lower-case `config.txt` is the same
/// directory entry with two "display lower case" flag bits set, and is opened
/// by this same upper-case name. The card carries it lower case, to match the
/// certificates beside it. `tools/fat-assumptions` proves the two are one
/// entry, for a directory as well as a file, rather than leaving it as the sort
/// of assumption that has cost this project a bench session before. A long name such as `teddiebox.conf` is not out of
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
/// The example in this module's docs (`update_url`) is 55 characters; 128
/// leaves room for a real path without being silly about it.
pub const MAX_UPDATE_URL: usize = 128;

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
    /// Whether holding an ear skips a chapter.
    ///
    /// A stock box's ears do volume and nothing else; skipping on a held ear
    /// is this box's own idea, which makes it taste rather than a fault to be
    /// fixed. It lives on the card so that changing your mind costs an edit on
    /// a laptop rather than a reflash.
    ///
    /// Defaults to `true`. A file that says nothing keeps the box doing what
    /// it already did — and with this off, a held ear simply steps the volume
    /// like any other press, which is what a stock box does.
    pub ears_skip: bool,
    /// Where to fetch the OTA manifest, in full — scheme, host, port and
    /// path, e.g.
    /// `https://teddycloud.local:8443/content/FIRMWARE/teddiebox.txt`.
    ///
    /// **Optional, and `None` means no OTA at all** — the box does not check
    /// for updates. A card written for an older firmware must not suddenly
    /// start fetching firmware images from somewhere, and there is no safe
    /// value to default to instead.
    ///
    /// Defaults to `None`.
    pub update_url: Option<String<MAX_UPDATE_URL>>,
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
    /// Unlike `ssid` and `server`, absence of `update_url` is not an error —
    /// it means no OTA. So an empty value cannot reuse `Missing*`-shaped
    /// handling; it is its own, honest mistake: `update_url =` with nothing
    /// after it, someone who meant to write a URL and didn't.
    EmptyUpdateUrl,
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

/// Which settings the bench has typed, so a later card read cannot undo them.
///
/// The card is read **lazily**, at the first mount, which can happen after
/// somebody has already typed an override — the mount is triggered by whatever
/// first needs the card, and on this box that is often a network command. So
/// "the card is read at boot, the console overrides it afterwards" is not true
/// in the order it happens, and replacing the whole config on a card read
/// silently undid the override. That cost an hour at the bench: certificates
/// were checked immediately after the box confirmed it would not check them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Overridden {
    pub ssid: bool,
    pub password: bool,
    pub server: bool,
    pub insecure: bool,
    pub ears_skip: bool,
    pub update_url: bool,
}

impl Overridden {
    /// Takes the card's value for every setting the bench has *not* typed.
    ///
    /// Per field rather than all-or-nothing: typing a passphrase must not pin
    /// the ssid to whatever happened to be in memory before the card was read.
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
            insecure: if self.insecure {
                held.insecure
            } else {
                card.insecure
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
        }
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
        let mut ears_skip = true;
        let mut update_url: Option<String<MAX_UPDATE_URL>> = None;

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
                "ears_skip" => {
                    ears_skip = parse_bool(strip_comment(value))?;
                }
                "update_url" => {
                    let value = strip_comment(value);
                    if value.is_empty() {
                        return Err(ConfigError::EmptyUpdateUrl);
                    }
                    update_url =
                        Some(String::try_from(value).map_err(|_| ConfigError::ValueTooLong)?);
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
            ears_skip,
            update_url,
        })
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::format;

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
    /// Hold-to-skip is this box's own idea — a stock box's ears do volume and
    /// nothing else — so whether it is wanted is taste, and taste belongs on
    /// the card rather than behind a reflash.
    #[test]
    fn a_missing_ears_skip_key_leaves_the_ears_skipping() {
        let c = Config::parse("ssid = A\nserver = s:1\n").unwrap();
        assert!(c.ears_skip);
    }

    #[test]
    fn ears_skip_no_makes_the_ears_volume_only() {
        let c = Config::parse("ssid = A\nserver = s:1\nears_skip = no\n").unwrap();
        assert!(!c.ears_skip);
    }

    /// Spelled out one form at a time. A test that looped over a list of
    /// truthy spellings would agree with whatever list the parser happened to
    /// hold, which is the failure these tables exist to catch.
    #[test]
    fn ears_skip_accepts_the_spellings_a_parent_might_reach_for() {
        for text in [
            "ssid = A\nserver = s:1\nears_skip = yes\n",
            "ssid = A\nserver = s:1\nears_skip = true\n",
            "ssid = A\nserver = s:1\nears_skip = TRUE\n",
            "ssid = A\nserver = s:1\nears_skip = 1\n",
        ] {
            assert!(Config::parse(text).unwrap().ears_skip, "{text}");
        }
        for text in [
            "ssid = A\nserver = s:1\nears_skip = no\n",
            "ssid = A\nserver = s:1\nears_skip = false\n",
            "ssid = A\nserver = s:1\nears_skip = False\n",
            "ssid = A\nserver = s:1\nears_skip = 0\n",
        ] {
            assert!(!Config::parse(text).unwrap().ears_skip, "{text}");
        }
    }

    /// A typo about the ears is refused rather than guessed, the same way
    /// `insecure = ture` is. Silently taking it as `false` would leave a
    /// parent believing they had switched something on.
    #[test]
    fn a_misspelled_ears_skip_value_is_refused() {
        assert_eq!(
            Config::parse("ssid = A\nserver = s:1\nears_skip = yse\n"),
            Err(ConfigError::MalformedValue)
        );
    }

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
    fn card() -> Config {
        Config::parse("ssid = FromCard\npassword = cardpw\nserver = card:1\ninsecure = no\n")
            .unwrap()
    }

    /// The bug this exists to prevent, caught on the bench: the card is read
    /// lazily, at the first mount, which can be *after* somebody has typed an
    /// override. Replacing the whole struct then silently undid it, and the box
    /// checked certificates the bench had just told it not to.
    #[test]
    fn a_later_card_read_does_not_undo_what_the_bench_set() {
        let mut held = card();
        held.insecure = true;
        let overridden = Overridden {
            insecure: true,
            ..Overridden::default()
        };

        let merged = overridden.merge(card(), &held);
        assert!(merged.insecure, "the card undid the override");
        assert_eq!(
            merged.ssid.as_str(),
            "FromCard",
            "and the rest still came from the card"
        );
    }

    /// Nothing overridden means the card wins outright, which is the ordinary
    /// boot.
    #[test]
    fn an_untouched_setting_is_taken_from_the_card() {
        let held = Config::parse("ssid = Old\nserver = old:1\n").unwrap();
        let merged = Overridden::default().merge(card(), &held);
        assert_eq!(merged.ssid.as_str(), "FromCard");
        assert_eq!(merged.server.as_str(), "card:1");
        assert!(!merged.insecure);
    }

    /// Each field is independent: overriding the passphrase must not pin the
    /// ssid to whatever was in RAM before the card was read.
    #[test]
    fn overrides_are_per_field() {
        let mut held = card();
        held.password = heapless::String::try_from("typed").unwrap();
        let overridden = Overridden {
            password: true,
            ..Overridden::default()
        };

        let merged = overridden.merge(card(), &held);
        assert_eq!(merged.password.as_str(), "typed");
        assert_eq!(merged.ssid.as_str(), "FromCard");
        assert_eq!(merged.server.as_str(), "card:1");
    }

    #[test]
    fn update_url_parses_when_present() {
        let c = Config::parse(
            "ssid = A\nserver = s:1\nupdate_url = https://teddycloud.local:8443/content/FIRMWARE/teddiebox.txt\n",
        )
        .unwrap();
        assert_eq!(
            c.update_url.as_deref(),
            Some("https://teddycloud.local:8443/content/FIRMWARE/teddiebox.txt")
        );
    }

    /// Absence means no OTA at all, not a default location to check. A card
    /// written for an older firmware must not suddenly start fetching images
    /// from somewhere, and there is no safe value to default to.
    #[test]
    fn a_missing_update_url_key_leaves_it_none() {
        let c = Config::parse("ssid = A\nserver = s:1\n").unwrap();
        assert_eq!(c.update_url, None);
    }

    #[test]
    fn an_overlong_update_url_is_rejected_rather_than_truncated() {
        // 143 characters, over the 128-byte limit.
        const TEXT: &str = "ssid = A\nserver = s:1\nupdate_url = https://example.com/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n";
        assert_eq!(Config::parse(TEXT), Err(ConfigError::ValueTooLong));
    }

    /// Pins the boundary itself, not just a value comfortably past it — so
    /// MAX_UPDATE_URL cannot drift (say, to 140) with this suite still green.
    #[test]
    fn an_update_url_at_exactly_the_limit_is_accepted() {
        let value = format!("https://example.com/{}", "x".repeat(MAX_UPDATE_URL - 20));
        assert_eq!(value.len(), MAX_UPDATE_URL);
        let text = format!("ssid = A\nserver = s:1\nupdate_url = {value}\n");
        let c = Config::parse(&text).unwrap();
        assert_eq!(c.update_url.as_deref(), Some(value.as_str()));
    }

    #[test]
    fn an_update_url_one_byte_over_the_limit_is_refused() {
        let value = format!("https://example.com/{}", "x".repeat(MAX_UPDATE_URL - 19));
        assert_eq!(value.len(), MAX_UPDATE_URL + 1);
        let text = format!("ssid = A\nserver = s:1\nupdate_url = {value}\n");
        assert_eq!(Config::parse(&text), Err(ConfigError::ValueTooLong));
    }

    /// `update_url =` with nothing after it is someone who meant to write a
    /// URL. This project has already been bitten once, in
    /// `teddiebox-ota::manifest`, by a present-but-empty value sailing
    /// through as though it meant "absent".
    #[test]
    fn an_empty_update_url_is_refused() {
        assert_eq!(
            Config::parse("ssid = A\nserver = s:1\nupdate_url =\n"),
            Err(ConfigError::EmptyUpdateUrl)
        );
    }

    #[test]
    fn a_whitespace_only_update_url_is_refused() {
        assert_eq!(
            Config::parse("ssid = A\nserver = s:1\nupdate_url =    \n"),
            Err(ConfigError::EmptyUpdateUrl)
        );
    }

    #[test]
    fn a_trailing_comment_is_not_part_of_the_update_url() {
        let c = Config::parse(
            "ssid = A\nserver = s:1\nupdate_url = https://teddycloud.local/teddiebox.txt # ours\n",
        )
        .unwrap();
        assert_eq!(
            c.update_url.as_deref(),
            Some("https://teddycloud.local/teddiebox.txt")
        );
    }

    /// Only a `#` that starts a word is a comment, matching `ssid` and
    /// `server`. A URL has no legitimate `#` in this use, but the rule is the
    /// same rule everywhere it applies.
    #[test]
    fn a_hash_inside_the_update_url_is_part_of_the_value() {
        let c = Config::parse("ssid = A\nserver = s:1\nupdate_url = https://x/y#z\n").unwrap();
        assert_eq!(c.update_url.as_deref(), Some("https://x/y#z"));
    }

    /// The bug this exists to prevent: overriding `insecure` at the console
    /// must not throw away an `update_url` typed at the same session.
    #[test]
    fn overridden_update_url_survives_a_later_card_read() {
        let mut held = card();
        held.update_url = Some(heapless::String::try_from("https://typed/teddiebox.txt").unwrap());
        let overridden = Overridden {
            update_url: true,
            ..Overridden::default()
        };

        let merged = overridden.merge(card(), &held);
        assert_eq!(
            merged.update_url.as_deref(),
            Some("https://typed/teddiebox.txt")
        );
        assert_eq!(merged.ssid.as_str(), "FromCard");
    }

    #[test]
    fn an_untouched_update_url_is_taken_from_the_card() {
        let mut typed_card = card();
        typed_card.update_url =
            Some(heapless::String::try_from("https://card/teddiebox.txt").unwrap());
        let held = Config::parse("ssid = Old\nserver = old:1\n").unwrap();

        let merged = Overridden::default().merge(typed_card, &held);
        assert_eq!(
            merged.update_url.as_deref(),
            Some("https://card/teddiebox.txt")
        );
    }
}
