//! Properties of the setup portal's parsers over whatever a device on its
//! access point sends: HTTP requests, DHCP datagrams and form bodies.
//!
//! The access point is open while the portal runs, so any device in range
//! can send anything. A panic resets the box out of setup mode; a config the
//! portal wrongly calls valid is written to the card.
//!
//! The seed is fixed, so a failure reproduces on every run. Finding new
//! inputs is the fuzzer's job, not the unit suite's.

use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};
use teddiebox_portal::submission::{examine, Submission};
use teddiebox_portal::{dhcp, form, http, MAX_BODY, MAX_CONFIG};

fn config() -> Config {
    Config {
        rng_seed: RngSeed::Fixed(0x7ed_d1e),
        failure_persistence: None,
        ..Config::default()
    }
}

/// A string from `chars`, `len` characters long.
fn text(
    chars: &'static str,
    len: std::ops::RangeInclusive<usize>,
) -> impl Strategy<Value = String> {
    let chars: Vec<char> = chars.chars().collect();
    prop::collection::vec(prop::sample::select(chars), len).prop_map(String::from_iter)
}

const PATH_CHARS: &str = "abcdefghijklmnopqrstuvwxyz0123456789/._-";
/// Header text, and what breaks it: separators, line breaks, non-ASCII.
const VALUE_CHARS: &str = "0123456789abcXYZ -:/\r\né";

/// A request-shaped text: a request line, headers the parser reads and some
/// it does not, and a blank line (or not), then some body.
fn request_like() -> impl Strategy<Value = Vec<u8>> {
    let name = prop::sample::select(vec!["Content-Length", "content-length", "Host", ""]);
    let header = (name, text(VALUE_CHARS, 0..=24)).prop_map(|(n, v)| format!("{n}: {v}\r\n"));
    (
        prop::sample::select(vec!["GET", "POST", "PUT", ""]),
        text(VALUE_CHARS, 0..=24),
        prop::collection::vec(header, 0..=5),
        prop::sample::select(vec!["\r\n", "", "\n"]),
        prop::collection::vec(any::<u8>(), 0..=32),
    )
        .prop_map(|(method, path, headers, end, body)| {
            let mut out = format!("{method} {path} HTTP/1.1\r\n").into_bytes();
            out.extend(headers.concat().into_bytes());
            out.extend(end.as_bytes());
            out.extend(body);
            out
        })
}

/// A well-formed request whose body has arrived one byte short of what it
/// announces, exactly, or one byte over: the edges of "complete".
fn request_with_body() -> impl Strategy<Value = Vec<u8>> {
    (0..=64usize, prop::sample::select(vec![-1i64, 0, 1])).prop_map(|(announced, off)| {
        let arrived = (announced as i64 + off).max(0) as usize;
        let mut out =
            format!("POST /config HTTP/1.1\r\nContent-Length: {announced}\r\n\r\n").into_bytes();
        out.resize(out.len() + arrived, b'x');
        out
    })
}

/// Field positions in a DHCP message (RFC 2131 section 2, figure 1).
const OP: usize = 0;
const XID: std::ops::Range<usize> = 4..8;
const FLAGS: std::ops::Range<usize> = 10..12;
/// The first six bytes of `chaddr`: an Ethernet MAC.
const CHADDR: std::ops::Range<usize> = 28..34;
const COOKIE: std::ops::Range<usize> = 236..240;
const BOOTREQUEST: u8 = 1;
const BOOTREPLY: u8 = 2;

/// The fixed part of a BOOTREQUEST: op, hardware type and length, the
/// transaction id, the flags and the client's MAC, then the magic cookie.
/// Everything else is zero.
fn bootrequest(xid: [u8; 4], flags: [u8; 2], chaddr: [u8; 6]) -> Vec<u8> {
    let mut out = vec![0u8; COOKIE.end];
    out[OP] = BOOTREQUEST;
    out[1] = 1; // Ethernet
    out[2] = 6; // MAC length
    out[XID].copy_from_slice(&xid);
    out[FLAGS].copy_from_slice(&flags);
    out[CHADDR].copy_from_slice(&chaddr);
    out[COOKIE].copy_from_slice(&[0x63, 0x82, 0x53, 0x63]);
    out
}

/// A client's DISCOVER or REQUEST, with padding and unknown options around
/// its message type, ended as RFC 2132 ends options.
fn client_message(
    xid: [u8; 4],
    flags: [u8; 2],
    chaddr: [u8; 6],
    discover: bool,
    before: &[Vec<u8>],
    after: &[Vec<u8>],
) -> Vec<u8> {
    let mut raw = bootrequest(xid, flags, chaddr);
    raw.extend(before.concat());
    raw.extend([53, 1, if discover { 1 } else { 3 }]);
    raw.extend(after.concat());
    raw.push(255);
    raw
}

/// A DHCP option (RFC 2132): padding, or a code, a length and that many
/// bytes. Never 53 (message type) or 255 (end), which the tests place.
fn dhcp_option() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        Just(vec![0u8]),
        (1..=252u8, prop::collection::vec(any::<u8>(), 0..=8)).prop_map(|(code, value)| {
            let code = if code == 53 { 54 } else { code };
            let mut out = vec![code, value.len() as u8];
            out.extend(value);
            out
        }),
    ]
}

/// A datagram shaped like a client's: a BOOTREQUEST with arbitrary options,
/// some cut short, or arbitrary bytes after the cookie.
fn datagram_like() -> impl Strategy<Value = Vec<u8>> {
    (
        any::<[u8; 4]>(),
        any::<[u8; 2]>(),
        any::<[u8; 6]>(),
        prop::collection::vec(
            prop_oneof![dhcp_option(), prop::collection::vec(any::<u8>(), 1..=4)],
            0..=8,
        ),
        0..=600usize,
    )
        .prop_map(|(xid, flags, chaddr, options, cut)| {
            let mut out = bootrequest(xid, flags, chaddr);
            out.extend(options.concat());
            out.truncate(cut.max(1));
            out
        })
}

/// Percent-encodes `value` as a browser does for a form: `+` for a space,
/// letters and digits as they are, everything else as `%XX` in either case.
fn form_encode(value: &[u8], upper: bool) -> String {
    value
        .iter()
        .map(|&b| match b {
            b' ' => "+".to_string(),
            b if b.is_ascii_alphanumeric() => (b as char).to_string(),
            b if upper => format!("%{b:02X}"),
            b => format!("%{b:02x}"),
        })
        .collect()
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn any_request_parses_or_is_refused_within_what_arrived(raw in request_like()) {
        // Given: anything shaped roughly like a request

        // When
        let parsed = http::parse(&raw);

        // Then: refused, or read within what arrived
        if let Ok(request) = parsed {
            prop_assert!(request.header_len <= raw.len());
            prop_assert!(raw[..request.header_len].ends_with(b"\r\n\r\n"));
            prop_assert!(request.content_length <= MAX_BODY);
        }
    }

    #[test]
    fn a_request_is_complete_once_its_whole_body_has_arrived(
        raw in prop_oneof![request_like(), request_with_body()],
    ) {
        // Given: a request, whole or not

        // When
        let complete = http::is_complete(&raw);

        // Then: complete exactly when the whole announced body is there
        match http::parse(&raw) {
            Ok(request) => {
                let arrived = raw.len() - request.header_len;
                prop_assert_eq!(complete, Ok(arrived >= request.content_length));
            }
            Err(http::RequestError::Incomplete) => prop_assert_eq!(complete, Ok(false)),
            Err(refused) => prop_assert_eq!(complete, Err(refused)),
        }
    }

    #[test]
    fn a_request_announcing_too_large_a_body_is_refused(
        path in text(PATH_CHARS, 1..=40).prop_map(|p| format!("/{p}")),
        length in prop_oneof![Just(MAX_BODY + 1), MAX_BODY + 1..=2 * MAX_BODY],
    ) {
        // Given
        let raw = format!("POST {path} HTTP/1.1\r\nContent-Length: {length}\r\n\r\n");

        // When
        let parsed = http::parse(raw.as_bytes());

        // Then
        prop_assert_eq!(parsed, Err(http::RequestError::TooLarge));
    }

    #[test]
    fn a_written_request_reads_back_as_written(
        post in any::<bool>(),
        path in text(PATH_CHARS, 1..=40).prop_map(|p| format!("/{p}")),
        length in prop_oneof![Just(MAX_BODY), 0..=MAX_BODY],
    ) {
        // Given
        let method = if post { "POST" } else { "GET" };
        let raw = format!("{method} {path} HTTP/1.1\r\nHost: 192.168.4.1\r\nContent-Length: {length}\r\n\r\n");

        // When
        let request = http::parse(raw.as_bytes()).unwrap();

        // Then
        prop_assert_eq!(request.method, if post { http::Method::Post } else { http::Method::Get });
        prop_assert_eq!(request.path, path.as_str());
        prop_assert_eq!(request.content_length, length);
        prop_assert_eq!(request.header_len, raw.len());
    }

    #[test]
    fn any_datagram_parses_or_is_ignored(raw in datagram_like()) {
        // Given: anything shaped roughly like a DHCP datagram

        // When
        let parsed = dhcp::parse(&raw);

        // Then: it returned, read or ignored, rather than panicking
        let _ = parsed;
    }

    #[test]
    fn a_client_message_is_read_as_sent(
        xid in any::<[u8; 4]>(),
        flags in any::<[u8; 2]>(),
        chaddr in any::<[u8; 6]>(),
        discover in any::<bool>(),
        before in prop::collection::vec(dhcp_option(), 0..=4),
        after in prop::collection::vec(dhcp_option(), 0..=4),
    ) {
        // Given
        let raw = client_message(xid, flags, chaddr, discover, &before, &after);

        // When
        let incoming = dhcp::parse(&raw).unwrap();

        // Then
        prop_assert_eq!(incoming.kind, if discover { dhcp::Kind::Discover } else { dhcp::Kind::Request });
        prop_assert_eq!(incoming.xid, xid);
        prop_assert_eq!(incoming.chaddr, chaddr);
        prop_assert_eq!(incoming.flags, flags);
    }

    #[test]
    fn the_answer_goes_to_the_client_that_asked(
        xid in any::<[u8; 4]>(),
        flags in any::<[u8; 2]>(),
        chaddr in any::<[u8; 6]>(),
        discover in any::<bool>(),
    ) {
        // Given
        let incoming = dhcp::parse(&client_message(xid, flags, chaddr, discover, &[], &[])).unwrap();

        // When
        let reply = dhcp::reply(&incoming, if discover { dhcp::Reply::Offer } else { dhcp::Reply::Ack });

        // Then
        prop_assert_eq!(reply[OP], BOOTREPLY);
        prop_assert_eq!(&reply[XID], &xid[..]);
        prop_assert_eq!(&reply[FLAGS], &flags[..]);
        prop_assert_eq!(&reply[CHADDR], &chaddr[..]);
    }

    #[test]
    fn a_form_field_decodes_to_what_was_typed(
        value in prop::collection::vec(any::<u8>(), 0..=64),
        other in prop::collection::vec(any::<u8>(), 0..=16),
        upper in any::<bool>(),
        first in any::<bool>(),
    ) {
        // Given: our field and a look-alike, in either order
        let mine = format!("config={}", form_encode(&value, upper));
        let theirs = format!("configuration={}", form_encode(&other, upper));
        let body = if first { format!("{mine}&{theirs}") } else { format!("{theirs}&{mine}") };

        // When
        let decoded: heapless::Vec<u8, 64> = form::field(body.as_bytes(), "config").unwrap();

        // Then
        prop_assert_eq!(&decoded[..], &value[..]);
    }

    #[test]
    fn any_form_body_decodes_or_is_refused(body in prop::collection::vec(any::<u8>(), 0..=128)) {
        // Given: any bytes as a form body

        // When
        let decoded: Result<heapless::Vec<u8, 64>, _> = form::field(&body, "config");

        // Then: it returned, decoded or refused, rather than panicking
        let _ = decoded;
    }

    #[test]
    fn only_a_config_that_parses_is_written_to_the_card(
        body in prop_oneof![
            prop::collection::vec(any::<u8>(), 0..=256),
            text("abc= \n#_:/.1\r", 0..=MAX_CONFIG).prop_map(|t| format!("config={}", form_encode(t.as_bytes(), false)).into_bytes()),
        ],
    ) {
        // Given: any body, or a form of config-like text

        // When
        let submission = examine(&body);

        // Then: whatever is cleared for the card is a config the box parses
        if let Submission::Write(bytes) = submission {
            let text = core::str::from_utf8(&bytes);
            prop_assert!(text.is_ok(), "wrote bytes that are not text");
            prop_assert!(teddiebox_config::Config::parse(text.unwrap()).is_ok(), "wrote a config that does not parse");
        }
    }
}
