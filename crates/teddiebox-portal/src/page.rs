//! The one page the portal serves.
//!
//! **Self-contained on purpose.** On the setup access point the phone has a
//! route to nothing, so anything this page referenced — a stylesheet, a font,
//! a script — would simply not load. Everything is inline, and the test
//! `the_page_loads_nothing_from_the_network` is what keeps it that way.

use heapless::Vec;

/// Room for the file, its escaping, and the markup around it.
pub const MAX_PAGE: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageError {
    /// The rendered page does not fit [`MAX_PAGE`].
    TooLong,
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

const FORM_OPEN: &str = "<form method=\"post\" action=\"/save\">\
<textarea name=\"config\" spellcheck=\"false\" autocapitalize=\"off\">";

const FORM_CLOSE: &str = "</textarea><button type=\"submit\">Save and restart</button>\
</form></body></html>";

/// Builds the page, with the card's bytes in the box and an error above it.
pub fn render(config: &[u8], error: Option<&str>) -> Result<Vec<u8, MAX_PAGE>, PageError> {
    let mut out = Vec::new();
    push(&mut out, HEAD.as_bytes())?;
    if let Some(message) = error {
        push(&mut out, b"<p class=\"error\">")?;
        escape(&mut out, message.as_bytes())?;
        push(&mut out, b"</p>")?;
    }
    push(&mut out, FORM_OPEN.as_bytes())?;
    escape(&mut out, config)?;
    push(&mut out, FORM_CLOSE.as_bytes())?;
    Ok(out)
}

fn escape(out: &mut Vec<u8, MAX_PAGE>, bytes: &[u8]) -> Result<(), PageError> {
    for &b in bytes {
        match b {
            b'&' => push(out, b"&amp;")?,
            b'<' => push(out, b"&lt;")?,
            b'>' => push(out, b"&gt;")?,
            b'"' => push(out, b"&quot;")?,
            other => out.push(other).map_err(|_| PageError::TooLong)?,
        }
    }
    Ok(())
}

fn push(out: &mut Vec<u8, MAX_PAGE>, bytes: &[u8]) -> Result<(), PageError> {
    out.extend_from_slice(bytes).map_err(|_| PageError::TooLong)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::form;
    use std::string::ToString;

    fn body_of(page: &[u8]) -> &str {
        core::str::from_utf8(page).unwrap()
    }

    #[test]
    fn the_file_appears_inside_the_textarea() {
        let page = render(b"ssid = HomeNet\n", None).unwrap();
        let text = body_of(&page);
        let open = text.find("<textarea").unwrap();
        let close = text.find("</textarea>").unwrap();
        assert!(text[open..close].contains("ssid = HomeNet"));
    }

    #[test]
    fn markup_in_the_file_is_escaped() {
        let page = render(b"ssid = <b>&\"x\"", None).unwrap();
        let text = body_of(&page);
        assert!(text.contains("&lt;b&gt;&amp;&quot;x&quot;"));
        assert!(!text.contains("<b>"));
    }

    #[test]
    fn a_missing_file_renders_an_empty_textarea() {
        let page = render(b"", None).unwrap();
        let text = body_of(&page);
        assert!(text.contains("<textarea"));
        let at = text.find("<textarea").unwrap();
        let open = at + text[at..].find('>').unwrap();
        assert!(text[open + 1..].starts_with("</textarea>"));
    }

    #[test]
    fn an_error_is_shown_when_there_is_one() {
        let page = render(b"", Some("ssid is missing")).unwrap();
        assert!(body_of(&page).contains("ssid is missing"));
    }

    #[test]
    fn no_error_text_appears_when_there_is_none() {
        let page = render(b"ssid = x\n", None).unwrap();
        assert!(!body_of(&page).contains("class=\"error\""));
    }

    #[test]
    fn the_page_loads_nothing_from_the_network() {
        let text = body_of(&render(b"ssid = x\n", None).unwrap()).to_string();
        for forbidden in ["http://", "https://", "<script", "<link", "<img"] {
            assert!(!text.contains(forbidden), "page reaches for {forbidden}");
        }
    }

    /// The box has no route anywhere on this network, so a password with `#`
    /// and `&` in it has to survive render and come back byte-identical.
    #[test]
    fn a_password_with_hash_and_ampersand_survives_the_round_trip() {
        let original = b"ssid = Home\npassword = a#b&c\nserver = box.lan:443\n";
        let page = render(original, None).unwrap();
        let text = body_of(&page);
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
            .collect::<std::string::String>()
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&amp;", "&");
        assert_eq!(unescaped.as_bytes(), original);
    }

    #[test]
    fn a_file_too_large_for_the_page_is_refused() {
        let huge = [b'x'; MAX_PAGE];
        assert_eq!(render(&huge, None).unwrap_err(), PageError::TooLong);
    }
}
