//! The one page the portal serves.
//!
//! **Self-contained on purpose.** On the setup access point the phone has a
//! route to nothing, so anything this page referenced — a stylesheet, a font,
//! a script — would simply not load. Everything is inline, and the test
//! `the_page_loads_nothing_from_the_network` is what keeps it that way.
//!
//! **Streamed rather than built.** An earlier version rendered into a single
//! `heapless::Vec` and had to pick a size for it; the size picked (4096) was
//! smaller than a full-size `CONFIG.TXT` can escape to (6144 plus markup), so
//! a file the box would happily save could not be shown back — a 413 for its
//! own file, with no way left to correct it. Growing the buffer to fit costs
//! 7.6 KB that would sit in `.bss` for the life of the firmware, and `.bss`
//! is precisely what this box has none of. So the page goes out in pieces
//! instead: [`length`] says how long it will be, [`pieces`] says what they
//! are, and [`escape_chunk`] hands the escaped runs over a few bytes at a
//! time. Nothing here holds a page.

use heapless::Vec;

/// How many [`Piece`]s a page is ever made of.
///
/// Head, the three of the error paragraph, the form's two halves and the
/// config between them.
const PIECES: usize = 7;

/// A stretch of the page, in the order it goes on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Piece<'a> {
    /// Markup, already safe, to be written as it is.
    Literal(&'a str),
    /// Content, to be escaped on its way out — see [`escape_chunk`].
    Escaped(&'a [u8]),
}

const HEAD: &str = "<!doctype html><html><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>teddiebox setup</title><style>\
body{font:16px system-ui;margin:0;padding:1rem;background:#f6f5f3;color:#1a1a1a}\
textarea{width:100%;height:60vh;font:14px ui-monospace,monospace;\
padding:.5rem;box-sizing:border-box}\
button{font:16px system-ui;padding:.6rem 1.2rem;margin-top:.75rem}\
.error{background:#fde8e6;border-left:4px solid #c0392b;padding:.6rem;\
margin-bottom:.75rem}\
</style></head><body><h1>config.txt</h1>";

const ERROR_OPEN: &str = "<p class=\"error\">";
const ERROR_CLOSE: &str = "</p>";

const FORM_OPEN: &str = "<form method=\"post\" action=\"/save\">\
<textarea name=\"config\" spellcheck=\"false\" autocapitalize=\"off\">";

const FORM_CLOSE: &str = "</textarea><button type=\"submit\">Save and restart</button>\
</form></body></html>";

/// The page, in the order it goes out.
///
/// The card's bytes go in the textarea and the error, if there is one, above
/// it. Neither is copied: the pieces borrow, and the caller writes them.
pub fn pieces<'a>(config: &'a [u8], error: Option<&'a str>) -> Vec<Piece<'a>, PIECES> {
    let mut out = Vec::new();
    // Every push below is one of the `PIECES` this is sized for, so none of
    // them can fail. `let _ =` rather than `unwrap`: a panic in a no_std
    // firmware is a box that goes silent, and there is nothing here to panic
    // about.
    let _ = out.push(Piece::Literal(HEAD));
    if let Some(message) = error {
        let _ = out.push(Piece::Literal(ERROR_OPEN));
        let _ = out.push(Piece::Escaped(message.as_bytes()));
        let _ = out.push(Piece::Literal(ERROR_CLOSE));
    }
    let _ = out.push(Piece::Literal(FORM_OPEN));
    let _ = out.push(Piece::Escaped(config));
    let _ = out.push(Piece::Literal(FORM_CLOSE));
    out
}

/// How many bytes [`pieces`] will produce once escaped.
///
/// Needed before the first of them is written, because it is the response's
/// `Content-Length` — which is why this counts rather than measuring: there is
/// no rendered page to measure.
pub fn length(config: &[u8], error: Option<&str>) -> usize {
    pieces(config, error)
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
/// Answers `(consumed, written)`: how far into `bytes` it got, and how much of
/// `out` it filled. An escape is never split across two calls — `out` is left
/// short rather than torn — so the caller can hand over any size of buffer and
/// call again from `consumed`.
///
/// An `out` shorter than the longest escape (`&quot;`, six bytes) can return
/// `(0, 0)` and make no progress. Callers pass a buffer larger than that; the
/// alternative would be to write a torn escape, which is worse than looping.
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

    /// The whole page, assembled the way the firmware assembles it.
    ///
    /// The chunk is deliberately eight bytes — barely more than the longest
    /// escape — so every test here runs the streaming across a boundary
    /// rather than in one comfortable pass.
    fn page_of(config: &[u8], error: Option<&str>) -> StdVec<u8> {
        let mut out = StdVec::new();
        for piece in pieces(config, error) {
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

    fn text_of(config: &[u8], error: Option<&str>) -> String {
        String::from_utf8(page_of(config, error)).unwrap()
    }

    #[test]
    fn the_file_appears_inside_the_textarea() {
        let text = text_of(b"ssid = HomeNet\n", None);
        let open = text.find("<textarea").unwrap();
        let close = text.find("</textarea>").unwrap();
        assert!(text[open..close].contains("ssid = HomeNet"));
    }

    #[test]
    fn markup_in_the_file_is_escaped() {
        let text = text_of(b"ssid = <b>&\"x\"", None);
        assert!(text.contains("&lt;b&gt;&amp;&quot;x&quot;"));
        assert!(!text.contains("<b>"));
    }

    #[test]
    fn a_missing_file_renders_an_empty_textarea() {
        let text = text_of(b"", None);
        assert!(text.contains("<textarea"));
        let at = text.find("<textarea").unwrap();
        let open = at + text[at..].find('>').unwrap();
        assert!(text[open + 1..].starts_with("</textarea>"));
    }

    #[test]
    fn an_error_is_shown_when_there_is_one() {
        assert!(text_of(b"", Some("ssid is missing")).contains("ssid is missing"));
    }

    #[test]
    fn no_error_text_appears_when_there_is_none() {
        assert!(!text_of(b"ssid = x\n", None).contains("class=\"error\""));
    }

    #[test]
    fn markup_in_the_error_is_escaped_too() {
        let text = text_of(b"", Some("<script>x</script>"));
        assert!(text.contains("&lt;script&gt;x&lt;/script&gt;"));
        assert!(!text.contains("<script"));
    }

    #[test]
    fn the_page_loads_nothing_from_the_network() {
        let text = text_of(b"ssid = x\n", None).to_string();
        for forbidden in ["http://", "https://", "<script", "<link", "<img"] {
            assert!(!text.contains(forbidden), "page reaches for {forbidden}");
        }
    }

    /// The `Content-Length` goes out before the first byte of the page does,
    /// so a count that disagrees with what is streamed leaves the phone
    /// waiting for bytes that never come, or reading into the next response.
    #[test]
    fn the_promised_length_is_the_length_that_arrives() {
        for (config, error) in [
            (&b""[..], None),
            (&b"ssid = x\n"[..], None),
            (&b"pass = a&b<c>d\"e\n"[..], Some("a & b")),
            (&[b'"'; 1024][..], Some("<<<")),
        ] {
            assert_eq!(length(config, error), page_of(config, error).len());
        }
    }

    /// The exact boundary this used to fail at: `MAX_CONFIG` bytes of the one
    /// byte escaping expands furthest. That is a file `form::field` decodes
    /// and `write_config` writes, so the page has to be able to show it back.
    #[test]
    fn the_largest_file_of_the_worst_bytes_is_shown_in_full() {
        let worst = [b'"'; crate::MAX_CONFIG];
        let text = text_of(&worst, None);
        let open = text.find("<textarea").unwrap();
        let open = open + text[open..].find('>').unwrap() + 1;
        let close = text.find("</textarea>").unwrap();
        assert_eq!(&text[open..close], "&quot;".repeat(crate::MAX_CONFIG));
    }

    /// An escape that would not fit is left for the next call rather than cut
    /// in half. Six bytes of room take one `&quot;`; five take none.
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

    /// The box has no route anywhere on this network, so a password with `#`
    /// and `&` in it has to survive the page and come back byte-identical.
    #[test]
    fn a_password_with_hash_and_ampersand_survives_the_round_trip() {
        let original = b"ssid = Home\npassword = a#b&c\nserver = box.lan:443\n";
        let text = text_of(original, None);
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
