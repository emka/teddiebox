//! What a submitted form means, decided before anything touches the card.
//!
//! The rule this exists to hold: **validate before writing, never after.** A
//! file the box will refuse at its next boot must not reach the card, because
//! the person who finds out is whoever picks up a box that no longer works,
//! with no clue why. That rule lived inside an `async fn` welded to a
//! `TcpSocket` and a mounted card, so nothing could check it ran in the right
//! order — or at all.

use crate::form;
use crate::MAX_CONFIG;
use heapless::Vec;

/// What the box should do about a submitted form.
///
/// The card is deliberately absent. Whether one is mounted is the caller's
/// question and is asked *after* this, so a config typed against a box with no
/// card in it is still decoded, still validated, and still handed back to the
/// person who typed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Submission {
    /// Refuse, with nothing in the textarea.
    ///
    /// For the cases where there are no bytes worth handing back: a form that
    /// did not decode, and one that is not text. Both are things a browser
    /// does not do, so nothing a person typed is lost by not echoing them.
    Refuse(&'static str),
    /// Hand these bytes back in the textarea, with this complaint.
    ///
    /// A typo then costs a correction rather than a retype.
    HandBack(Vec<u8, MAX_CONFIG>, &'static str),
    /// Parses. These bytes are safe to put on the card.
    Write(Vec<u8, MAX_CONFIG>),
}

/// Decodes a `POST /save` body and says what it means.
pub fn examine(body: &[u8]) -> Submission {
    // One arm per variant: `TooLong` is the only one of the three that is
    // about the *file* rather than about the request carrying it, and it is
    // the one somebody can do something about. Saying "did not arrive intact"
    // to a config that is simply too big sends them looking at their phone
    // instead of at their file.
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

/// Says what went wrong in words somebody can act on.
///
/// One arm per variant and no catch-all, so a new
/// [`teddiebox_config::ConfigError`] makes this fail to compile rather than
/// quietly telling everybody "invalid".
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

    /// Percent-encodes the way a browser's form post does, so the tests drive
    /// the decoder rather than going round it.
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

    /// The rule this module exists for. A config missing the one line the box
    /// cannot boot without must come back to whoever typed it, not go to the
    /// card and surface as a box that no longer works.
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

    /// What was typed comes back, so a typo costs a correction rather than a
    /// retype. The bytes echoed are the submitted ones, not a re-rendering.
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

    /// A body carrying no `config` field at all is not something a browser
    /// sends, so there is nothing of anybody's to hand back.
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

    /// Distinguished from a broken form on purpose: this one is about the
    /// file, and it is the one somebody can do something about.
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
