//! Properties of what the box reads from an update server and from its card.
//!
//! The manifest and the image arrive over the network, from whoever controls
//! the server. The one guarantee the update path makes (see the trust model in
//! `url.rs`) is that the server is always the host from the card's
//! `update_url`, and that a manifest can choose a path on it, never a host.
//!
//! The seed is fixed, so a failure reproduces on every run. Finding new
//! inputs is the fuzzer's job, not the unit suite's.

use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};
use teddiebox_ota::{image_version, resolve_image, split, Manifest, OtaError, MAX_MANIFEST};

fn config() -> Config {
    Config {
        rng_seed: RngSeed::Fixed(0x7ed_d1e),
        failure_persistence: None,
        ..Config::default()
    }
}

/// The bytes a request line may carry, as documented on `resolve_image`.
fn is_path_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/' | b'~')
}

fn is_host_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':')
}

/// A string from `chars`, `len` characters long.
fn text(
    chars: &'static str,
    len: std::ops::RangeInclusive<usize>,
) -> impl Strategy<Value = String> {
    let chars: Vec<char> = chars.chars().collect();
    prop::collection::vec(prop::sample::select(chars), len).prop_map(String::from_iter)
}

/// Safe characters, and the ones a hostile or careless value would bring:
/// separators, a scheme's colon, userinfo's `@`, whitespace and line breaks
/// that would split a request line, escapes, and non-ASCII.
const ANY_URL_CHARS: &str = "abcXYZ019._-~/:@?#% \r\n\té";
const PATH_CHARS: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-~";
const HOST_CHARS: &str = "abcdefghijklmnopqrstuvwxyz0123456789.-:";
const VERSION_CHARS: &str = "abcdefghijklmnopqrstuvwxyz0123456789.-_+";

/// A manifest path as `split` produces one: absolute, naming a file.
fn manifest_path() -> impl Strategy<Value = String> {
    prop::collection::vec(text(PATH_CHARS, 1..=12), 1..=4).prop_map(|segments| {
        let mut path = String::new();
        for segment in segments {
            path.push('/');
            path.push_str(&segment);
        }
        path
    })
}

/// A manifest-shaped text: lines of known and unknown keys with arbitrary
/// values, comments and blank lines, in any order.
fn manifest_like() -> impl Strategy<Value = String> {
    let key = prop::sample::select(vec!["version", "sha256", "length", "image", "other", ""]);
    let line = prop_oneof![
        (key, text(ANY_URL_CHARS, 0..=80)).prop_map(|(k, v)| format!("{k} = {v}")),
        text(ANY_URL_CHARS, 0..=40),
        Just("# a comment".to_string()),
        Just(String::new()),
    ];
    prop::collection::vec(line, 0..=10).prop_map(|lines| lines.join("\n"))
}

#[derive(Debug, Clone)]
struct Fields {
    version: String,
    sha256: [u8; 32],
    length: u32,
    image: String,
}

fn fields() -> impl Strategy<Value = Fields> {
    (
        text(VERSION_CHARS, 1..=31),
        any::<[u8; 32]>(),
        any::<u32>(),
        text(PATH_CHARS, 1..=64),
    )
        .prop_map(|(version, sha256, length, image)| Fields {
            version,
            sha256,
            length,
            image,
        })
}

/// Writes `fields` in the documented `key = value` form, in the given line
/// order, with a comment and a blank line among them.
fn render(fields: &Fields, order: &[usize], upper_hex: bool) -> String {
    let sha: String = fields.sha256.iter().map(|b| format!("{b:02x}")).collect();
    let sha = if upper_hex { sha.to_uppercase() } else { sha };
    let lines = [
        format!("version = {}", fields.version),
        format!("sha256  = {sha}"),
        format!("length={}", fields.length),
        format!("  image   =   {}  ", fields.image),
    ];
    let mut out = String::from("# teddiebox update\n\n");
    for &i in order {
        out.push_str(&lines[i]);
        out.push('\n');
    }
    out
}

/// Where the ESP-IDF application descriptor starts in an image, and the
/// magic word that opens it (esp_app_format.h).
const DESCRIPTOR_OFFSET: usize = 0x20;
const DESCRIPTOR_MAGIC: u32 = 0xABCD_5432;
/// The descriptor's `version` field is a `char[32]`.
const VERSION_FIELD: usize = 32;

/// An ESP-IDF image head: random bytes, with the descriptor's magic word in
/// place in half the cases so the version field is reached.
fn image_head() -> impl Strategy<Value = Vec<u8>> {
    (prop::collection::vec(any::<u8>(), 0..=128), any::<bool>()).prop_map(|(mut head, magic)| {
        let magic_end = DESCRIPTOR_OFFSET + 4;
        if magic && head.len() >= magic_end {
            head[DESCRIPTOR_OFFSET..magic_end].copy_from_slice(&DESCRIPTOR_MAGIC.to_le_bytes());
        }
        head
    })
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn any_manifest_text_parses_or_is_refused(text in manifest_like()) {
        let _ = Manifest::parse_read(text.as_bytes(), MAX_MANIFEST);
    }

    #[test]
    fn a_manifest_that_is_not_text_is_refused_as_such(
        fields in fields(),
        at in any::<prop::sample::Index>(),
        junk in prop::sample::select(vec![0xFFu8, 0xC0, 0x80]),
    ) {
        let mut raw = render(&fields, &[0, 1, 2, 3], false).into_bytes();
        raw.insert(at.index(raw.len() + 1), junk);
        prop_assert_eq!(Manifest::parse_read(&raw, MAX_MANIFEST), Err(OtaError::NotText));
    }

    #[test]
    fn a_written_manifest_reads_back_as_written(
        fields in fields(),
        order in Just(vec![0usize, 1, 2, 3]).prop_shuffle(),
        upper_hex in any::<bool>(),
    ) {
        let text = render(&fields, &order, upper_hex);
        let parsed = Manifest::parse_read(text.as_bytes(), MAX_MANIFEST).unwrap();
        prop_assert_eq!(parsed.version.as_str(), fields.version.as_str());
        prop_assert_eq!(parsed.sha256, fields.sha256);
        prop_assert_eq!(parsed.length, fields.length);
        prop_assert_eq!(parsed.image.as_str(), fields.image.as_str());
    }

    #[test]
    fn a_resolved_image_is_a_safe_absolute_path_on_the_same_server(
        manifest in manifest_path(),
        image in text(ANY_URL_CHARS, 0..=40),
    ) {
        if let Ok(path) = resolve_image(&manifest, &image) {
            prop_assert!(path.starts_with('/'), "not absolute: {path:?}");
            prop_assert!(path.bytes().all(is_path_byte), "unsafe byte in {path:?}");
            prop_assert!(!path.split('/').any(|s| s == ".."), "climbs out: {path:?}");
            prop_assert!(path.ends_with(image.trim_start_matches('/')), "lost the image: {path:?}");
        }
    }

    #[test]
    fn the_host_comes_from_the_url_and_nowhere_else(
        scheme in prop::sample::select(vec!["https://", "http://", "HTTPS://", ""]),
        host in prop_oneof![text(HOST_CHARS, 1..=20), text(ANY_URL_CHARS, 0..=20)],
        path in prop_oneof![manifest_path(), text(ANY_URL_CHARS, 0..=40)],
    ) {
        let url = format!("{scheme}{host}{path}");
        if let Ok(parts) = split(&url) {
            let named = url["https://".len()..].split('/').next().unwrap();
            let expected = if named.contains(':') { named.to_string() } else { format!("{named}:443") };
            prop_assert_eq!(parts.host.as_str(), expected.as_str());
            prop_assert!(parts.host.bytes().all(is_host_byte), "unsafe host {:?}", parts.host);
            prop_assert!(parts.path.starts_with('/'), "not absolute: {:?}", parts.path);
            prop_assert!(parts.path.bytes().all(is_path_byte), "unsafe path {:?}", parts.path);
        }
    }

    #[test]
    fn any_image_head_yields_a_version_or_is_refused(head in image_head()) {
        if let Ok(version) = image_version(&head) {
            prop_assert!(version.len() <= VERSION_FIELD);
            prop_assert!(!version.contains('\0'));
        }
    }
}
