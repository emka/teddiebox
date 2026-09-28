//! Properties of the response-head parser over whatever a server, or
//! something pretending to be one, sends back.
//!
//! The body is Opus written straight to the card at the offset the head
//! implies, so the parser must never panic, never claim a body starts beyond
//! what arrived, and read the fields a well-formed head carries exactly.
//!
//! A parser that loops for ever hangs these tests rather than failing them,
//! as it would hang the box; the fuzzer's time limit is what reports a loop.
//!
//! The seed is fixed, so a failure reproduces on every run. Finding new
//! inputs is the fuzzer's job, not the unit suite's.

use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};
use teddiebox_cloud::{parse_head, CloudError, ContentRange, MAX_ETAG};

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

/// What header values are made of, and what breaks them: separators, the
/// characters of a range, line breaks, and non-ASCII.
const VALUE_CHARS: &str = "0123456789abcXYZ -/*:\"\r\né";

/// A head-shaped text: a status line, then headers the parser reads and some
/// it does not, with arbitrary values, and a blank line (or not) at the end.
fn head_like() -> impl Strategy<Value = Vec<u8>> {
    let name = prop::sample::select(vec![
        "Content-Length",
        "content-range",
        "ETag",
        "Transfer-Encoding",
        "X-Other",
        "",
    ]);
    let header = (name, text(VALUE_CHARS, 0..=40)).prop_map(|(n, v)| format!("{n}: {v}\r\n"));
    let status = prop_oneof![100..=599u16, any::<u16>()];
    (
        prop::sample::select(vec!["HTTP/1.1", "HTTP/1.0", "HTTP", ""]),
        status,
        prop::collection::vec(header, 0..=6),
        prop::sample::select(vec!["\r\n", "", "\n"]),
        prop::collection::vec(any::<u8>(), 0..=32),
    )
        .prop_map(|(version, status, headers, end, body)| {
            let mut out = format!("{version} {status} Reason\r\n").into_bytes();
            out.extend(headers.concat().into_bytes());
            out.extend(end.as_bytes());
            out.extend(body);
            out
        })
}

/// A well-formed final head and what it states.
#[derive(Debug, Clone)]
struct Head {
    status: u16,
    content_length: Option<u32>,
    content_range: Option<ContentRange>,
    etag: Option<String>,
}

fn head() -> impl Strategy<Value = Head> {
    let range = (any::<u32>(), any::<u32>(), prop::option::of(any::<u32>()))
        .prop_map(|(first, last, total)| ContentRange { first, last, total });
    (
        200..=599u16,
        prop::option::of(any::<u32>()),
        prop::option::of(range),
        prop::option::of(text("abcdefXYZ0123456789-\"/", 1..=MAX_ETAG)),
    )
        .prop_map(|(status, content_length, content_range, etag)| Head {
            status,
            content_length,
            content_range,
            etag,
        })
}

/// Writes `head` the way RFC 9110 spells each field.
fn render(head: &Head) -> String {
    let mut out = format!("HTTP/1.1 {} Whatever\r\n", head.status);
    if let Some(length) = head.content_length {
        out.push_str(&format!("Content-Length: {length}\r\n"));
    }
    if let Some(range) = head.content_range {
        let total = range.total.map_or("*".to_string(), |t| t.to_string());
        out.push_str(&format!(
            "Content-Range: bytes {}-{}/{total}\r\n",
            range.first, range.last
        ));
    }
    if let Some(etag) = &head.etag {
        out.push_str(&format!("ETag: {etag}\r\n"));
    }
    out.push_str("\r\n");
    out
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn any_head_parses_or_is_refused_and_the_body_starts_after_it(raw in head_like()) {
        if let Ok((_, body_at)) = parse_head(&raw) {
            prop_assert!(body_at <= raw.len(), "body at {body_at} of {}", raw.len());
            prop_assert!(raw[..body_at].ends_with(b"\r\n\r\n"), "body does not follow a blank line");
        }
    }

    #[test]
    fn a_head_that_is_not_text_is_refused(
        head in head(),
        at in any::<prop::sample::Index>(),
        junk in prop::sample::select(vec![0xFFu8, 0xC0, 0x80]),
    ) {
        let mut raw = render(&head).into_bytes();
        let head_len = raw.len() - 4;
        raw.insert(at.index(head_len), junk);
        prop_assert_eq!(parse_head(&raw), Err(CloudError::MalformedResponse));
    }

    #[test]
    fn a_written_head_reads_back_as_written(
        head in head(),
        preambles in 0..=3usize,
        body in prop::collection::vec(any::<u8>(), 0..=32),
    ) {
        let mut raw = "HTTP/1.1 100 Continue\r\n\r\n".repeat(preambles).into_bytes();
        raw.extend(render(&head).into_bytes());
        let body_starts = raw.len();
        raw.extend(&body);

        let (parsed, body_at) = parse_head(&raw).unwrap();
        prop_assert_eq!(parsed.status, head.status);
        prop_assert_eq!(parsed.content_length, head.content_length);
        prop_assert_eq!(parsed.content_range, head.content_range);
        prop_assert_eq!(parsed.etag.as_deref(), head.etag.as_deref());
        prop_assert_eq!(body_at, body_starts);
    }
}
