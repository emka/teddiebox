//! Takes an `update_url` apart, and puts a manifest's `image` path back
//! together against it.
//!
//! `CONFIG.TXT`'s `update_url` is the manifest's full URL (scheme, host,
//! path), as copied from a browser's address bar.
//! `teddiebox_cloud::build_path_request` needs the host and path separately,
//! and a manifest's `image` is relative to the manifest, not to the server
//! root. A mistake here would fetch the wrong file, so it is tested on the
//! host.
//!
//! ## Trust model
//!
//! There is no code signing. `decide` only compares version strings, and the
//! digest check uses a hash from the *same manifest*, so whoever can write
//! the manifest decides what gets flashed.
//!
//! The one guarantee: the TLS server is always the host from the card's
//! `update_url`, never one named in the manifest. A manifest can choose the
//! *path* on that host, never the *server*. Code that fetches the image must
//! keep it that way: a manifest's `image` must never become a scheme or a
//! host.

use crate::OtaError;
use heapless::String;

/// Longest host (with port) accepted.
pub const MAX_HOST: usize = 64;

/// Longest path accepted, whether the manifest's own path or an image
/// resolved against it.
pub const MAX_PATH: usize = 96;

/// An `update_url` taken apart into the two pieces a request needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateUrl {
    /// `host:port`, as `build_path_request`'s `server` argument wants it.
    pub host: String<MAX_HOST>,
    /// Absolute path to the manifest, leading slash included.
    pub path: String<MAX_PATH>,
}

/// Splits an `update_url` into a host and a manifest path.
///
/// The scheme must be `https://`. teddyCloud only speaks TLS, and a plain
/// HTTP request to it hangs instead of failing, so an `http://` URL would
/// make the box look frozen. Refusing it here gives a clear error.
pub fn split(update_url: &str) -> Result<UpdateUrl, OtaError> {
    const SCHEME: &str = "https://";

    let rest = update_url.strip_prefix(SCHEME).ok_or(OtaError::NotHttps)?;

    // The URL names the manifest in full, so there must be a `/` after the
    // host.
    let slash = rest.find('/').ok_or(OtaError::MalformedUrl)?;

    let host = &rest[..slash];
    if host.is_empty() || !is_host_port(host) {
        return Err(OtaError::MalformedUrl);
    }

    // `split_server` (firmware/src/tls.rs) needs `host:port` and does not
    // guess a port. A URL copied from a browser often has no port, so add
    // `:443`, the HTTPS default.
    //
    // The length is checked after adding the port, since that can push it
    // over MAX_HOST.
    let mut host = String::<MAX_HOST>::try_from(host).map_err(|_| OtaError::ValueTooLong)?;
    if !host.contains(':') {
        host.push_str(":443").map_err(|_| OtaError::ValueTooLong)?;
    }

    let path = &rest[slash..];
    // A path ending in `/` names a directory, not a manifest.
    if path.ends_with('/') {
        return Err(OtaError::MalformedUrl);
    }
    // The same check `resolve_image` applies to the manifest's part. The
    // resolved image path joins both parts, so both must be checked. It also
    // refuses a `#fragment`, which is never sent to a server, and a query
    // string, which no server here expects.
    if !is_path_safe(path) {
        return Err(OtaError::MalformedUrl);
    }

    Ok(UpdateUrl {
        host,
        path: String::try_from(path).map_err(|_| OtaError::ValueTooLong)?,
    })
}

/// Says whether a value may be interpolated into a request line.
///
/// Allows only ASCII letters, digits and `._-/~`, which is enough for a
/// file name. This rules out spaces and CRs, which could add tokens or line
/// breaks to the request line.
fn is_path_safe(value: &str) -> bool {
    value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/' | b'~'))
}

/// Says whether a value is shaped like the `host:port` a `Host:` header wants.
///
/// Like [`is_path_safe`], for the host: letters, digits, `.`, `-` and `_`,
/// plus the `:` before a port.
fn is_host_port(value: &str) -> bool {
    value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':'))
}

/// Resolves a manifest's `image` value against the manifest's own path.
///
/// An absolute `image` (leading `/`) is used unchanged. Otherwise `image` is
/// joined to `manifest_path`'s directory — everything up to and including its
/// last `/` — the same rule a browser applies to a relative link.
///
/// Refuses any `image` with a `..` path **segment**. This only catches
/// mistakes; it is not a security boundary (see "Trust model" above), since
/// an absolute `image` can name any path anyway. It checks whole segments,
/// so a name like `teddiebox..bin` is allowed.
pub fn resolve_image(manifest_path: &str, image: &str) -> Result<String<MAX_PATH>, OtaError> {
    if image.is_empty() {
        return Err(OtaError::MalformedUrl);
    }
    // `build_path_request` copies the path into the request line without
    // escaping.
    if !is_path_safe(image) {
        return Err(OtaError::MalformedUrl);
    }
    if image.split('/').any(|segment| segment == "..") {
        return Err(OtaError::MalformedUrl);
    }

    let mut out = String::<MAX_PATH>::new();

    if let Some(absolute) = image.strip_prefix('/') {
        out.push('/').map_err(|_| OtaError::ValueTooLong)?;
        out.push_str(absolute).map_err(|_| OtaError::ValueTooLong)?;
    } else {
        // `split` always returns a leading slash, so this only fails for
        // other callers. Without a directory, the result would be an invalid
        // relative request line (`GET teddiebox.bin HTTP/1.1`).
        let dir_end = manifest_path
            .rfind('/')
            .map(|i| i + 1)
            .ok_or(OtaError::MalformedUrl)?;
        out.push_str(&manifest_path[..dir_end])
            .map_err(|_| OtaError::ValueTooLong)?;
        out.push_str(image).map_err(|_| OtaError::ValueTooLong)?;
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::format;

    use super::*;

    #[test]
    fn splits_the_real_example_into_host_and_path() {
        let u = split("https://teddycloud.local:8443/content/FIRMWARE/teddiebox.txt").unwrap();
        assert_eq!(u.host.as_str(), "teddycloud.local:8443");
        assert_eq!(u.path.as_str(), "/content/FIRMWARE/teddiebox.txt");
    }

    // `split_server` (firmware/src/tls.rs) needs a port, so the HTTPS default
    // is added.
    #[test]
    fn splits_a_url_with_no_port() {
        let u = split("https://teddycloud.local/teddiebox.txt").unwrap();
        assert_eq!(u.host.as_str(), "teddycloud.local:443");
        assert_eq!(u.path.as_str(), "/teddiebox.txt");
    }

    #[test]
    fn refuses_plain_http() {
        assert_eq!(
            split("http://teddycloud.local/teddiebox.txt"),
            Err(OtaError::NotHttps)
        );
    }

    #[test]
    fn refuses_a_url_with_no_scheme_at_all() {
        assert_eq!(split("teddycloud.local/teddiebox.txt"), Err(OtaError::NotHttps));
    }

    #[test]
    fn refuses_a_host_with_no_slash_after_it() {
        assert_eq!(
            split("https://teddycloud.local:8443"),
            Err(OtaError::MalformedUrl)
        );
    }

    #[test]
    fn refuses_a_path_ending_in_slash() {
        assert_eq!(
            split("https://teddycloud.local:8443/content/FIRMWARE/"),
            Err(OtaError::MalformedUrl)
        );
    }

    /// The host goes into `Host:` and the path into the request line, both
    /// unescaped. A `CR` would add a header; a space would add a token.
    #[test]
    fn refuses_a_host_carrying_a_bare_cr() {
        assert_eq!(
            split("https://teddycloud.local\rX-Thing: 1:8443/teddiebox.txt"),
            Err(OtaError::MalformedUrl)
        );
    }

    #[test]
    fn refuses_a_path_carrying_a_space() {
        assert_eq!(
            split("https://teddycloud.local:8443/ted diebox.txt"),
            Err(OtaError::MalformedUrl)
        );
    }

    #[test]
    fn refuses_an_empty_host() {
        assert_eq!(split("https:///teddiebox.txt"), Err(OtaError::MalformedUrl));
    }

    #[test]
    fn refuses_a_host_longer_than_max_host() {
        // 65 'a's, one past MAX_HOST.
        let host = "a".repeat(MAX_HOST + 1);
        let url = format!("https://{host}/teddiebox.txt");
        assert_eq!(split(&url), Err(OtaError::ValueTooLong));
    }

    #[test]
    fn refuses_a_path_longer_than_max_path() {
        // MAX_PATH 'a's after the leading slash, one past MAX_PATH overall.
        let path = "a".repeat(MAX_PATH);
        let url = format!("https://teddycloud.local/{path}");
        assert_eq!(split(&url), Err(OtaError::ValueTooLong));
    }

    // The host alone fits under MAX_HOST, but adding the default port pushes
    // it one byte over.
    #[test]
    fn refuses_a_host_that_only_overflows_once_the_default_port_is_added() {
        // 61 'a's + ":443" is 65, one past MAX_HOST (64).
        let host = "a".repeat(61);
        let url = format!("https://{host}/teddiebox.txt");
        assert_eq!(split(&url), Err(OtaError::ValueTooLong));
    }

    // A URL copied from a browser may have a `#fragment`, which must never be
    // sent. No query string is expected either.
    #[test]
    fn refuses_a_path_with_a_fragment() {
        assert_eq!(
            split("https://teddycloud.local:8443/x.txt#frag"),
            Err(OtaError::MalformedUrl)
        );
    }

    #[test]
    fn refuses_a_path_with_a_query_string() {
        assert_eq!(
            split("https://teddycloud.local:8443/x.txt?y=1"),
            Err(OtaError::MalformedUrl)
        );
    }

    #[test]
    fn resolve_image_joins_a_bare_filename_to_the_manifest_directory() {
        let p = resolve_image("/content/FIRMWARE/teddiebox.txt", "teddiebox.bin").unwrap();
        assert_eq!(p.as_str(), "/content/FIRMWARE/teddiebox.bin");
    }

    #[test]
    fn resolve_image_uses_an_absolute_image_unchanged() {
        let p = resolve_image("/content/FIRMWARE/teddiebox.txt", "/other/teddiebox.bin").unwrap();
        assert_eq!(p.as_str(), "/other/teddiebox.bin");
    }

    #[test]
    fn resolve_image_refuses_a_leading_dotdot_segment() {
        assert_eq!(
            resolve_image("/content/FIRMWARE/teddiebox.txt", "../secret.bin"),
            Err(OtaError::MalformedUrl)
        );
    }

    #[test]
    fn resolve_image_refuses_a_dotdot_segment_in_the_middle() {
        assert_eq!(
            resolve_image("/content/FIRMWARE/teddiebox.txt", "a/../../b.bin"),
            Err(OtaError::MalformedUrl)
        );
    }

    #[test]
    fn resolve_image_refuses_a_trailing_dotdot_segment() {
        assert_eq!(
            resolve_image("/content/FIRMWARE/teddiebox.txt", "a/.."),
            Err(OtaError::MalformedUrl)
        );
    }

    #[test]
    fn resolve_image_accepts_a_filename_that_merely_contains_dots() {
        let p = resolve_image("/content/FIRMWARE/teddiebox.txt", "teddiebox..bin").unwrap();
        assert_eq!(p.as_str(), "/content/FIRMWARE/teddiebox..bin");
    }

    #[test]
    fn resolve_image_refuses_an_empty_image() {
        assert_eq!(
            resolve_image("/content/FIRMWARE/teddiebox.txt", ""),
            Err(OtaError::MalformedUrl)
        );
    }

    // The path goes into the request line unescaped. A space would add a
    // token, as `image = a.bin HTTP/1.1` in the manifest would.
    #[test]
    fn resolve_image_refuses_a_space() {
        assert_eq!(
            resolve_image("/content/FIRMWARE/teddiebox.txt", "a.bin HTTP/1.1"),
            Err(OtaError::MalformedUrl)
        );
    }

    // Some HTTP parsers treat a lone CR as a line break, which could be used
    // to inject a request. (`str::lines` already removes LF.)
    #[test]
    fn resolve_image_refuses_a_bare_carriage_return() {
        assert_eq!(
            resolve_image("/content/FIRMWARE/teddiebox.txt", "a\rb.bin"),
            Err(OtaError::MalformedUrl)
        );
    }

    #[test]
    fn resolve_image_refuses_a_percent_sign() {
        assert_eq!(
            resolve_image("/content/FIRMWARE/teddiebox.txt", "a%2e.bin"),
            Err(OtaError::MalformedUrl)
        );
    }

    #[test]
    fn resolve_image_joins_at_a_manifest_path_at_the_root() {
        let p = resolve_image("/teddiebox.txt", "teddiebox.bin").unwrap();
        assert_eq!(p.as_str(), "/teddiebox.bin");
    }

    // `split` always returns a leading slash, but other callers might not.
    // Returning "teddiebox.bin" would build an invalid relative request line.
    #[test]
    fn resolve_image_refuses_a_manifest_path_with_no_slash_at_all() {
        assert_eq!(
            resolve_image("m.txt", "teddiebox.bin"),
            Err(OtaError::MalformedUrl)
        );
    }

    // A path that is too long is refused, not cut short.
    #[test]
    fn resolve_image_refuses_a_join_that_overflows_max_path() {
        // The manifest directory alone (93 bytes) fits under MAX_PATH (96);
        // appending the 5-byte image pushes the join past it.
        let manifest_path = format!("/{}/m.txt", "a".repeat(MAX_PATH - 5));
        assert_eq!(
            resolve_image(&manifest_path, "b.bin"),
            Err(OtaError::ValueTooLong)
        );
    }
}
