//! The one page the portal serves.
//!
//! **Self-contained.** On the setup access point the phone cannot reach the
//! internet, so external stylesheets, fonts or scripts would not load.
//! Everything is inline; `the_page_loads_nothing_from_the_network` checks
//! this.
//!
//! **Streamed, not built in memory.** A full-size `CONFIG.TXT` can escape to
//! over 6 KB, and there is no spare RAM for a buffer that size. So
//! [`length`] says how long the page will be, [`pieces`] lists its parts,
//! and [`escape_chunk`] escapes them a few bytes at a time.

use heapless::Vec;

/// How many [`Piece`]s a page is ever made of.
///
/// The head, three for each message paragraph, three for the config form
/// around the config, three for the certificate section around its
/// description, and the restart form.
const PIECES: usize = 1 + 3 * MESSAGES + 3 + 3 + 1;

/// How many messages a page shows: one about the request, one about the card.
pub const MESSAGES: usize = 2;

/// A stretch of the page, in the order it goes on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Piece<'a> {
    /// Markup, written as it is.
    Literal(&'a str),
    /// Content, escaped as it is written. See [`escape_chunk`].
    Escaped(&'a [u8]),
}

const HEAD: &str = "<!doctype html><html><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>teddiebox setup</title><style>\
body{font:16px system-ui;margin:0;padding:1rem;background:#f6f5f3;color:#1a1a1a}\
textarea{width:100%;height:60vh;font:14px ui-monospace,monospace;\
padding:.5rem;box-sizing:border-box}\
button{font:16px system-ui;padding:.6rem 1.2rem;margin:.75rem .5rem 0 0}\
input{margin-top:.75rem}\
.error{background:#fde8e6;border-left:4px solid #c0392b;padding:.6rem;\
margin-bottom:.75rem}\
.notice{background:#e6f4ea;border-left:4px solid #2e7d32;padding:.6rem;\
margin-bottom:.75rem}\
</style></head><body><h1>config.txt</h1>";

/// A line shown above the form: what went wrong, or what was done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message<'a> {
    Error(&'a str),
    Notice(&'a str),
}

const ERROR_OPEN: &str = "<p class=\"error\">";
const NOTICE_OPEN: &str = "<p class=\"notice\">";
const MESSAGE_CLOSE: &str = "</p>";

const CONFIG_OPEN: &str = "<form method=\"post\" action=\"/config\">\
<textarea name=\"config\" spellcheck=\"false\" autocapitalize=\"off\">";

const CONFIG_CLOSE: &str = "</textarea><button type=\"submit\">Write config.txt</button>\
</form>";

const CA_OPEN: &str = "<h1>certificate</h1><p>cert/tcca.der: ";

const CA_CLOSE: &str = "</p><form method=\"post\" action=\"/ca\" enctype=\"multipart/form-data\">\
<input type=\"file\" name=\"ca\" accept=\".der\">\
<button type=\"submit\">Write certificate</button></form>";

const RESTART: &str = "<form method=\"post\" action=\"/restart\">\
<button type=\"submit\">Restart</button></form></body></html>";

/// What the page says about the card's `cert/tcca.der`.
pub fn ca_status(card: CaOnCard) -> heapless::String<CA_STATUS> {
    use core::fmt::Write;

    let mut out = heapless::String::new();
    let _ = match card {
        CaOnCard::Certificate(n) => write!(out, "{n} bytes"),
        CaOnCard::Missing => write!(out, "missing"),
        CaOnCard::TooLarge => write!(out, "too large"),
        CaOnCard::Unreadable => write!(out, "unreadable"),
        CaOnCard::NotACertificate(n) => write!(out, "{n} bytes, not a certificate"),
    };
    out
}

/// Room for the longest [`ca_status`]: a size no larger than
/// [`crate::MAX_BODY`] and ", not a certificate".
pub const CA_STATUS: usize = 40;

/// What a request just wrote to the card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Written {
    Config,
    Ca,
}

/// What the request path says was just written, if anything.
pub fn written(path: &str) -> Option<Written> {
    match path.strip_prefix("/?written=")? {
        "config" => Some(Written::Config),
        "ca" => Some(Written::Ca),
        _ => None,
    }
}

/// How the notice after a certificate write begins; a [`ca_status`] follows.
const CA_WRITTEN: &str = "certificate written, ";

/// Room for the longest [`notice`].
pub const NOTICE: usize = CA_WRITTEN.len() + CA_STATUS;

/// What the page says after `done`, given what the card now holds.
pub fn notice(done: Written, card: CaOnCard) -> heapless::String<NOTICE> {
    use core::fmt::Write;

    let mut out = heapless::String::new();
    let _ = match done {
        Written::Config => write!(out, "config.txt written"),
        Written::Ca => write!(out, "{CA_WRITTEN}{}", ca_status(card)),
    };
    out
}

/// What the card holds at `cert/tcca.der`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaOnCard {
    /// A certificate, this many bytes long.
    Certificate(usize),
    /// No file there, or no card.
    Missing,
    /// A file larger than any certificate the box can hold.
    TooLarge,
    /// A file the card would not give back.
    Unreadable,
    /// A file this many bytes long that does not parse as a certificate,
    /// such as one cut short by a failed write.
    NotACertificate(usize),
}

/// The page, in the order it goes out.
///
/// The card's bytes go in the textarea, with the first [`MESSAGES`] of
/// `messages` above it, in order. `ca` describes the card's certificate, as
/// [`ca_status`] words it. Nothing is copied: the pieces borrow, and the
/// caller writes them.
pub fn pieces<'a>(
    config: &'a [u8],
    messages: &[Message<'a>],
    ca: &'a str,
) -> Vec<Piece<'a>, PIECES> {
    let mut out = Vec::new();
    // The vector has room for all `PIECES`, so no push can fail. `let _ =`
    // rather than `unwrap`, to avoid a panic path in the firmware.
    let _ = out.push(Piece::Literal(HEAD));
    for message in messages.iter().take(MESSAGES) {
        let (open, text) = match *message {
            Message::Error(text) => (ERROR_OPEN, text),
            Message::Notice(text) => (NOTICE_OPEN, text),
        };
        let _ = out.push(Piece::Literal(open));
        let _ = out.push(Piece::Escaped(text.as_bytes()));
        let _ = out.push(Piece::Literal(MESSAGE_CLOSE));
    }
    let _ = out.push(Piece::Literal(CONFIG_OPEN));
    let _ = out.push(Piece::Escaped(config));
    let _ = out.push(Piece::Literal(CONFIG_CLOSE));
    let _ = out.push(Piece::Literal(CA_OPEN));
    let _ = out.push(Piece::Escaped(ca.as_bytes()));
    let _ = out.push(Piece::Literal(CA_CLOSE));
    let _ = out.push(Piece::Literal(RESTART));
    out
}

/// How many bytes [`pieces`] will produce once escaped.
///
/// Needed before anything is written, for the `Content-Length` header.
pub fn length(config: &[u8], messages: &[Message<'_>], ca: &str) -> usize {
    pieces(config, messages, ca)
        .iter()
        .map(|piece| match piece {
            Piece::Literal(text) => text.len(),
            Piece::Escaped(bytes) => escaped_len(bytes),
        })
        .sum()
}

/// What `bytes` becomes once escaped, in bytes.
pub fn escaped_len(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .map(|&b| expansion(b).map_or(1, str::len))
        .sum()
}

/// Escapes as much of `bytes` as fits `out`.
///
/// Returns `(consumed, written)`: how much of `bytes` was used, and how much
/// of `out` was filled. An escape is never split across two calls, so the
/// caller can use any buffer size and call again from `consumed`.
///
/// If `out` is shorter than the longest escape (`&quot;`, six bytes), this
/// may return `(0, 0)` and make no progress, so callers must pass a larger
/// buffer.
pub fn escape_chunk(bytes: &[u8], out: &mut [u8]) -> (usize, usize) {
    let mut consumed = 0;
    let mut written = 0;
    for &b in bytes {
        match expansion(b) {
            Some(text) => {
                if written + text.len() > out.len() {
                    break;
                }
                out[written..written + text.len()].copy_from_slice(text.as_bytes());
                written += text.len();
            }
            None => {
                if written == out.len() {
                    break;
                }
                out[written] = b;
                written += 1;
            }
        }
        consumed += 1;
    }
    (consumed, written)
}

/// What one byte becomes in HTML, or `None` if it goes through as it is.
fn expansion(b: u8) -> Option<&'static str> {
    match b {
        b'&' => Some("&amp;"),
        b'<' => Some("&lt;"),
        b'>' => Some("&gt;"),
        b'"' => Some("&quot;"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::form;
    use std::string::{String, ToString};
    use std::vec::Vec as StdVec;

    /// The whole page, assembled the way the firmware does it.
    ///
    /// The chunk is only eight bytes, just over the longest escape, so every
    /// test crosses chunk boundaries.
    fn page_of(config: &[u8], messages: &[Message<'_>], ca: &str) -> StdVec<u8> {
        let mut out = StdVec::new();
        for piece in pieces(config, messages, ca) {
            match piece {
                Piece::Literal(text) => out.extend_from_slice(text.as_bytes()),
                Piece::Escaped(bytes) => {
                    let mut at = 0;
                    let mut chunk = [0u8; 8];
                    while at < bytes.len() {
                        let (consumed, written) = escape_chunk(&bytes[at..], &mut chunk);
                        assert!(consumed > 0, "no progress");
                        at += consumed;
                        out.extend_from_slice(&chunk[..written]);
                    }
                }
            }
        }
        out
    }

    fn text_of(config: &[u8], message: Option<Message<'_>>, ca: &str) -> String {
        String::from_utf8(page_of(config, message.as_slice(), ca)).unwrap()
    }

    #[test]
    fn the_file_appears_inside_the_textarea() {
        let text = text_of(b"ssid = HomeNet\n", None, "missing");
        let open = text.find("<textarea").unwrap();
        let close = text.find("</textarea>").unwrap();
        assert!(text[open..close].contains("ssid = HomeNet"));
    }

    #[test]
    fn the_config_form_posts_to_config() {
        // Given
        let config = b"";

        // When
        let text = text_of(config, None, "missing");

        // Then
        assert!(text.contains("<form method=\"post\" action=\"/config\">"));
    }

    #[test]
    fn a_notice_is_shown_apart_from_an_error() {
        // Given
        let message = Message::Notice("config.txt written");

        // When
        let text = text_of(b"", Some(message), "missing");

        // Then
        assert!(text.contains("<p class=\"notice\">config.txt written</p>"));
        assert!(!text.contains("class=\"error\""));
    }

    #[test]
    fn the_write_button_does_not_promise_a_restart() {
        // Given
        let config = b"";

        // When
        let text = text_of(config, None, "missing");

        // Then
        assert!(text.contains(">Write config.txt</button>"));
        assert!(!text.contains("Save and restart"));
    }

    #[test]
    fn restarting_is_its_own_post() {
        // Given
        let config = b"";

        // When
        let text = text_of(config, None, "missing");

        // Then
        assert!(text.contains(
            "<form method=\"post\" action=\"/restart\"><button type=\"submit\">Restart</button></form>"
        ));
    }

    #[test]
    fn the_certificate_form_is_a_file_upload_to_ca() {
        // Given
        let config = b"";

        // When
        let text = text_of(config, None, "missing");

        // Then
        assert!(text.contains(
            "<form method=\"post\" action=\"/ca\" enctype=\"multipart/form-data\">\
<input type=\"file\" name=\"ca\" accept=\".der\">\
<button type=\"submit\">Write certificate</button></form>"
        ));
    }

    #[test]
    fn the_cards_certificate_is_described() {
        // Given
        let ca = "787 bytes";

        // When
        let text = text_of(b"", None, ca);

        // Then
        assert!(text.contains("cert/tcca.der: 787 bytes"));
    }

    #[test]
    fn the_certificate_description_is_escaped() {
        // Given
        let ca = "<b>";

        // When
        let text = text_of(b"", None, ca);

        // Then
        assert!(text.contains("cert/tcca.der: &lt;b&gt;"));
    }

    #[test]
    fn a_certificate_on_the_card_is_described_by_its_size() {
        // Given
        let card = CaOnCard::Certificate(787);

        // When
        let status = ca_status(card);

        // Then
        assert_eq!(status.as_str(), "787 bytes");
    }

    #[test]
    fn no_certificate_on_the_card_is_described_as_missing() {
        // Given
        let card = CaOnCard::Missing;

        // When
        let status = ca_status(card);

        // Then
        assert_eq!(status.as_str(), "missing");
    }

    #[test]
    fn a_file_too_large_for_the_box_is_described_as_such() {
        // Given
        let card = CaOnCard::TooLarge;

        // When
        let status = ca_status(card);

        // Then
        assert_eq!(status.as_str(), "too large");
    }

    #[test]
    fn a_file_that_will_not_read_is_described_as_unreadable() {
        // Given
        let card = CaOnCard::Unreadable;

        // When
        let status = ca_status(card);

        // Then
        assert_eq!(status.as_str(), "unreadable");
    }

    #[test]
    fn a_file_that_is_not_a_certificate_is_described_by_its_size_and_why() {
        // Given
        let card = CaOnCard::NotACertificate(1536);

        // When
        let status = ca_status(card);

        // Then
        assert_eq!(status.as_str(), "1536 bytes, not a certificate");
    }

    #[test]
    fn two_messages_are_both_shown_in_order() {
        // Given
        let messages = [
            Message::Error("that is not a certificate"),
            Message::Error("the box could not read its card"),
        ];

        // When
        let text = String::from_utf8(page_of(b"", &messages, "missing")).unwrap();

        // Then
        let first = text.find("that is not a certificate").unwrap();
        let second = text.find("the box could not read its card").unwrap();
        assert!(first < second);
    }

    #[test]
    fn messages_beyond_the_page_s_room_do_not_cut_the_page_short() {
        // Given
        let messages = [
            Message::Error("one"),
            Message::Error("two"),
            Message::Error("three"),
        ];

        // When
        let text = String::from_utf8(page_of(b"", &messages, "missing")).unwrap();

        // Then
        assert!(text.ends_with("</body></html>"));
    }

    #[test]
    fn the_page_after_a_config_write_knows_it_was_written() {
        // Given
        let path = "/?written=config";

        // When
        let done = written(path);

        // Then
        assert_eq!(done, Some(Written::Config));
    }

    #[test]
    fn the_page_after_a_certificate_write_knows_it_was_written() {
        // Given
        let path = "/?written=ca";

        // When
        let done = written(path);

        // Then
        assert_eq!(done, Some(Written::Ca));
    }

    #[test]
    fn a_page_asked_for_plainly_or_with_an_unknown_value_says_nothing_was_written() {
        // Given
        let paths = ["/", "/?written=", "/?written=everything", "/?other=config"];

        // When
        let done = paths.map(written);

        // Then
        assert_eq!(done, [None, None, None, None]);
    }

    #[test]
    fn after_a_config_write_the_page_says_so() {
        // Given
        let done = Written::Config;

        // When
        let text = notice(done, CaOnCard::Certificate(787));

        // Then
        assert_eq!(text, "config.txt written");
    }

    #[test]
    fn after_a_certificate_write_the_page_says_how_large_it_is() {
        // Given
        let done = Written::Ca;

        // When
        let text = notice(done, CaOnCard::Certificate(787));

        // Then
        assert_eq!(text, "certificate written, 787 bytes");
    }

    #[test]
    fn markup_in_the_file_is_escaped() {
        let text = text_of(b"ssid = <b>&\"x\"", None, "missing");
        assert!(text.contains("&lt;b&gt;&amp;&quot;x&quot;"));
        assert!(!text.contains("<b>"));
    }

    #[test]
    fn a_missing_file_renders_an_empty_textarea() {
        let text = text_of(b"", None, "missing");
        assert!(text.contains("<textarea"));
        let at = text.find("<textarea").unwrap();
        let open = at + text[at..].find('>').unwrap();
        assert!(text[open + 1..].starts_with("</textarea>"));
    }

    #[test]
    fn an_error_is_shown_when_there_is_one() {
        assert!(
            text_of(b"", Some(Message::Error("ssid is missing")), "missing")
                .contains("ssid is missing")
        );
    }

    #[test]
    fn no_error_text_appears_when_there_is_none() {
        assert!(!text_of(b"ssid = x\n", None, "missing").contains("class=\"error\""));
    }

    #[test]
    fn markup_in_the_error_is_escaped_too() {
        let text = text_of(b"", Some(Message::Error("<script>x</script>")), "missing");
        assert!(text.contains("&lt;script&gt;x&lt;/script&gt;"));
        assert!(!text.contains("<script"));
    }

    #[test]
    fn the_page_loads_nothing_from_the_network() {
        let text = text_of(b"ssid = x\n", None, "missing").to_string();
        for forbidden in ["http://", "https://", "<script", "<link", "<img"] {
            assert!(!text.contains(forbidden), "page reaches for {forbidden}");
        }
    }

    /// `Content-Length` is sent before the page, so it must match exactly, or
    /// the phone would wait for missing bytes or read too far.
    #[test]
    fn the_promised_length_is_the_length_that_arrives() {
        for (config, message) in [
            (&b""[..], None),
            (&b"ssid = x\n"[..], None),
            (&b"pass = a&b<c>d\"e\n"[..], Some(Message::Error("a & b"))),
            (&b"ssid = x\n"[..], Some(Message::Notice("a & b"))),
            (&[b'"'; 1024][..], Some(Message::Error("<<<"))),
        ] {
            assert_eq!(
                length(config, message.as_slice(), "missing"),
                page_of(config, message.as_slice(), "missing").len()
            );
            assert_eq!(
                length(config, message.as_slice(), "787 bytes"),
                page_of(config, message.as_slice(), "787 bytes").len()
            );
        }
    }

    /// `MAX_CONFIG` bytes of the character that escapes longest. The box can
    /// save such a file, so the page must be able to show it.
    #[test]
    fn the_largest_file_of_the_worst_bytes_is_shown_in_full() {
        let worst = [b'"'; crate::MAX_CONFIG];
        let text = text_of(&worst, None, "missing");
        let open = text.find("<textarea").unwrap();
        let open = open + text[open..].find('>').unwrap() + 1;
        let close = text.find("</textarea>").unwrap();
        assert_eq!(&text[open..close], "&quot;".repeat(crate::MAX_CONFIG));
    }

    /// An escape that does not fit is left for the next call, not cut in
    /// half. Six bytes of room fit one `&quot;`; five fit none.
    #[test]
    fn an_escape_is_never_split_across_a_chunk() {
        let mut out = [0u8; 6];
        assert_eq!(escape_chunk(b"\"\"", &mut out), (1, 6));
        assert_eq!(&out[..6], b"&quot;");

        let mut tight = [0u8; 5];
        assert_eq!(escape_chunk(b"\"", &mut tight), (0, 0));
    }

    #[test]
    fn a_plain_byte_fills_the_last_place_in_a_chunk() {
        let mut out = [0u8; 3];
        assert_eq!(escape_chunk(b"abcd", &mut out), (3, 3));
        assert_eq!(&out[..], b"abc");
    }

    #[test]
    fn what_escaping_will_cost_is_counted_before_it_is_done() {
        assert_eq!(escaped_len(b"a&b"), 7);
        assert_eq!(escaped_len(b"\"\""), 12);
        assert_eq!(escaped_len(b"plain"), 5);
    }

    /// A password with `#` and `&` must survive the page and come back
    /// unchanged.
    #[test]
    fn a_password_with_hash_and_ampersand_survives_the_round_trip() {
        let original = b"ssid = Home\npassword = a#b&c\nserver = box.lan:443\n";
        let text = text_of(original, None, "missing");
        let at = text.find("<textarea").unwrap();
        let open = at + text[at..].find('>').unwrap() + 1;
        let close = text.find("</textarea>").unwrap();
        let shown = &text[open..close];

        // What a browser sends back for that textarea content.
        let mut posted = heapless::Vec::<u8, 512>::new();
        posted.extend_from_slice(b"config=").unwrap();
        for c in shown.bytes() {
            match c {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' => posted.push(c).unwrap(),
                b' ' => posted.push(b'+').unwrap(),
                other => {
                    posted.push(b'%').unwrap();
                    for d in [other >> 4, other & 0xf] {
                        posted.push(b"0123456789ABCDEF"[d as usize]).unwrap();
                    }
                }
            }
        }

        let decoded: heapless::Vec<u8, 512> = form::field(&posted, "config").unwrap();
        let unescaped = decoded
            .iter()
            .map(|&b| b as char)
            .collect::<String>()
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&amp;", "&");
        assert_eq!(unescaped.as_bytes(), original);
    }
}
