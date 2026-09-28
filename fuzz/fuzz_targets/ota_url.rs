//! The card's `update_url` and a manifest's `image`, which together decide
//! the request line the box sends.
//!
//! Input is `update_url`, a NUL, then `image`. Whatever the input, the host
//! comes only from the URL, the path is one a request line can carry, and
//! `image` cannot climb with a `..` segment. The card is trusted, so a `..`
//! in `update_url` itself is its owner's choice.

#![no_main]

use libfuzzer_sys::fuzz_target;
use teddiebox_ota::{resolve_image, split};

/// The bytes a request line may carry, as documented on `resolve_image`.
fn is_path_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/' | b'~')
}

fn is_host_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':')
}

fuzz_target!(|data: &[u8]| {
    let Ok(text) = core::str::from_utf8(data) else {
        return;
    };
    let (url, image) = text.split_once('\0').unwrap_or((text, ""));
    let Ok(parts) = split(url) else { return };
    assert!(
        parts.host.bytes().all(is_host_byte),
        "host {:?}",
        parts.host
    );
    assert!(
        parts.path.bytes().all(is_path_byte),
        "path {:?}",
        parts.path
    );
    assert!(parts.path.starts_with('/'), "path {:?}", parts.path);
    if let Ok(path) = resolve_image(&parts.path, image) {
        assert!(path.starts_with('/'), "image path {path:?}");
        assert!(path.bytes().all(is_path_byte), "image path {path:?}");
        assert!(!image.split('/').any(|s| s == ".."), "image {image:?}");
    }
});
