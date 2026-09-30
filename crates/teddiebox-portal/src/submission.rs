//! Decides what a submitted form means, before anything is written to the
//! card.
//!
//! **Validate before writing.** A file the box would refuse at its next boot
//! must never reach the card, or the box would stop working with no clue
//! why. Keeping this separate from the socket and the card makes the rule
//! testable.

use crate::form;
use crate::MAX_CONFIG;
use heapless::Vec;

/// What the box should do about a submitted form.
///
/// The card is not involved. The caller checks for a card *after* this, so a
/// config typed on a box without a card is still decoded, checked, and shown
/// back to the person who typed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Submission {
    /// Refuse, with nothing in the textarea.
    ///
    /// For a form that did not decode or is not text. A browser does not send
    /// either, so nothing typed is lost.
    Refuse(&'static str),
    /// Hand these bytes back in the textarea, with this complaint.
    ///
    /// So a typo only needs correcting, not retyping.
    HandBack(Vec<u8, MAX_CONFIG>, &'static str),
    /// Parses. These bytes are safe to put on the card.
    Write(Vec<u8, MAX_CONFIG>),
}

/// Decodes a `POST /config` body and says what it means.
pub fn examine(body: &[u8]) -> Submission {
    // One arm per variant: `TooLong` is about the *file*, not the request,
    // so it gets its own message pointing at the file.
    let submitted: Vec<u8, MAX_CONFIG> = match form::field(body, "config") {
        Ok(bytes) => bytes,
        Err(form::FormError::TooLong) => {
            return Submission::Refuse("that config is longer than the box will hold")
        }
        Err(form::FormError::NotFound) | Err(form::FormError::BadEscape) => {
            return Submission::Refuse("that form did not arrive intact")
        }
    };

    let Ok(text) = core::str::from_utf8(&submitted) else {
        return Submission::Refuse("that is not text");
    };

    match teddiebox_config::Config::parse(text) {
        Ok(_) => Submission::Write(submitted),
        Err(trouble) => Submission::HandBack(submitted, describe(trouble)),
    }
}

/// Explains what went wrong in words a person can act on.
///
/// No catch-all arm, so a new [`teddiebox_config::ConfigError`] variant fails
/// to compile until it has a message.
pub fn describe(trouble: teddiebox_config::ConfigError) -> &'static str {
    use teddiebox_config::ConfigError::*;
    match trouble {
        MissingSsid => "no ssid line — the box needs a network name",
        MissingServer => "no server line — the box needs somewhere to fetch from",
        ValueTooLong => "one of those values is too long for the box to hold",
        MalformedLine => "a line without an = on it",
        MalformedValue => "a key was given a value it does not accept",
        Truncated => "that config is longer than the box will read",
        NotText => "that is not text",
        EmptyUpdateUrl => "update_url is there but empty — give it a URL or remove it",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec as StdVec;

    /// Percent-encodes like a browser's form post, so the tests go through
    /// the decoder.
    fn posted(config: &str) -> StdVec<u8> {
        let mut body = StdVec::from(&b"config="[..]);
        for byte in config.as_bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => {
                    body.push(*byte)
                }
                b' ' => body.push(b'+'),
                other => {
                    body.extend_from_slice(alloc::format!("%{other:02X}").as_bytes());
                }
            }
        }
        body
    }

    const GOOD: &str = "ssid = homenet\npassword = hunter2\nserver = teddycloud.local\n";

    #[test]
    fn a_config_the_box_can_parse_is_cleared_for_the_card() {
        assert_eq!(
            examine(&posted(GOOD)),
            Submission::Write(Vec::from_slice(GOOD.as_bytes()).unwrap())
        );
    }

    /// A config missing a required line comes back to the person who typed
    /// it, and is not written to the card.
    #[test]
    fn a_config_the_box_would_refuse_at_boot_never_reaches_the_card() {
        let no_server = "ssid = homenet\npassword = hunter2\n";

        match examine(&posted(no_server)) {
            Submission::HandBack(bytes, why) => {
                assert_eq!(
                    bytes,
                    Vec::<u8, MAX_CONFIG>::from_slice(no_server.as_bytes()).unwrap()
                );
                assert_eq!(
                    why,
                    "no server line — the box needs somewhere to fetch from"
                );
            }
            other => panic!("a config with no server was not handed back: {other:?}"),
        }
    }

    /// The submitted bytes come back unchanged, so a typo only needs
    /// correcting.
    #[test]
    fn a_rejected_config_comes_back_exactly_as_it_was_typed() {
        let malformed = "ssid = homenet\nthis line has no equals\nserver = teddycloud.local\n";

        match examine(&posted(malformed)) {
            Submission::HandBack(bytes, why) => {
                assert_eq!(core::str::from_utf8(&bytes).unwrap(), malformed);
                assert_eq!(why, "a line without an = on it");
            }
            other => panic!("expected the config back in the textarea: {other:?}"),
        }
    }

    /// A body with no `config` field is not something a browser sends, so
    /// there is nothing to hand back.
    #[test]
    fn a_body_with_no_config_field_is_refused_with_an_empty_textarea() {
        assert_eq!(
            examine(b"something=else"),
            Submission::Refuse("that form did not arrive intact")
        );
    }

    #[test]
    fn a_body_whose_escaping_is_broken_is_refused_as_a_broken_form() {
        assert_eq!(
            examine(b"config=ssid%ZZnope"),
            Submission::Refuse("that form did not arrive intact")
        );
    }

    /// Separate from a broken form: this one is about the file, which the
    /// person can fix.
    #[test]
    fn a_config_too_long_for_the_box_blames_the_file_and_not_the_form() {
        let huge = "x".repeat(MAX_CONFIG + 1);

        assert_eq!(
            examine(&posted(&huge)),
            Submission::Refuse("that config is longer than the box will hold")
        );
    }

    #[test]
    fn bytes_that_are_not_text_are_refused_rather_than_handed_back() {
        // A lone continuation byte: valid percent-encoding, invalid UTF-8.
        assert_eq!(
            examine(b"config=ssid%80"),
            Submission::Refuse("that is not text")
        );
    }
}
