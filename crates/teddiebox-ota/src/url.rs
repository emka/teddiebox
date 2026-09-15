//! Takes an `update_url` apart, and puts a manifest's `image` path back
//! together against it.
//!
//! `CONFIG.TXT`'s `update_url` names the manifest in full — scheme, host,
//! path — because that is what a parent copies out of a browser bar.
//! `teddiebox_cloud::build_path_request` wants the host and the path
//! separately, and a manifest's `image` key is relative to the manifest, not
//! to the server root. Both joins are pure decision logic with no I/O, which
//! is why they live here rather than at the call site: a wrong join silently
//! fetches the wrong file or 404s, and that is worth testing without a box.

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
/// The scheme must be `https://`. This is not fussiness: the teddyCloud on
/// this LAN is TLS-only, and a plain-HTTP request to it hangs rather than
/// failing — so an `http://` URL would present as a box that has frozen, not
/// as a box with a bad config. Refusing here names the problem where someone
/// can read it, instead of leaving it to be diagnosed from a bench.
pub fn split(update_url: &str) -> Result<UpdateUrl, OtaError> {
    const SCHEME: &str = "https://";

    let rest = update_url.strip_prefix(SCHEME).ok_or(OtaError::NotHttps)?;

    // The key is specified as naming the manifest in full, so there must be
    // a `/` after the host at all.
    let slash = rest.find('/').ok_or(OtaError::MalformedUrl)?;

    let host = &rest[..slash];
    if host.is_empty() {
        return Err(OtaError::MalformedUrl);
    }

    // `split_server` (firmware/src/tls.rs) requires a `host:port` string and
    // refuses to guess a port itself, so a host copied without one — the
    // ordinary shape for a default-port URL out of a browser bar — would
    // parse clean here and then be unable to connect. The scheme is fixed at
    // `https`, so the port to fill in is known.
    //
    // Length is checked after the port is appended, not before: a host that
    // fits under MAX_HOST on its own can still overflow once `:443` is
    // added, and checking first would accept a value that cannot be stored.
    let mut host = String::<MAX_HOST>::try_from(host).map_err(|_| OtaError::ValueTooLong)?;
    if !host.contains(':') {
        host.push_str(":443").map_err(|_| OtaError::ValueTooLong)?;
    }

    let path = &rest[slash..];
    // A path ending in `/` names a directory, not a manifest.
    if path.ends_with('/') {
        return Err(OtaError::MalformedUrl);
    }
    // A fragment never leaves the client, and this key never carried a
    // query string; either riding along into a request line sent verbatim
    // is not what the person who pasted this URL meant.
    if path.contains('#') || path.contains('?') {
        return Err(OtaError::MalformedUrl);
    }

    Ok(UpdateUrl {
        host,
        path: String::try_from(path).map_err(|_| OtaError::ValueTooLong)?,
    })
}

/// Resolves a manifest's `image` value against the manifest's own path.
///
/// An absolute `image` (leading `/`) is used unchanged. Otherwise `image` is
/// joined to `manifest_path`'s directory — everything up to and including its
/// last `/` — the same rule a browser applies to a relative link.
///
/// Refuses any `image` containing a `..` path **segment**: traversal is
/// meaningless for a fetch relative to a fixed manifest, is a sign of a
/// manifest nobody should be acting on, and what this gates is a flash write.
/// The check is segment-based, not substring-based, so a filename that merely
/// contains two dots — `teddiebox..bin` — is not mistaken for one.
pub fn resolve_image(manifest_path: &str, image: &str) -> Result<String<MAX_PATH>, OtaError> {
    if image.is_empty() {
        return Err(OtaError::MalformedUrl);
    }
    // `build_path_request` interpolates the resolved path into an HTTP
    // request line unescaped, so any byte `image` contributes lands on the
    // wire as-is. A conservative allow-list -- ASCII letters, digits, and
    // `._-/~`, which is enough for a filename on this server -- is cheaper
    // and safer than trying to enumerate everything that is dangerous. It
    // also subsumes the space and the CR that would otherwise let a
    // manifest value add tokens or a stray line terminator to the request
    // line.
    if !image
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/' | b'~'))
    {
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
        let dir_end = manifest_path.rfind('/').map_or(0, |i| i + 1);
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

    // `split_server` (firmware/src/tls.rs) requires a colon in its `server`
    // argument and never guesses a port, so a host with none left as-is
    // parses clean here and then cannot connect. The scheme is fixed at
    // `https`, so the default port is known — fill it in.
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

    // The host alone fits under MAX_HOST, but the default port that gets
    // appended pushes it one byte over. Checking the length before the port
    // is filled in would let this through with a host that cannot actually
    // be stored.
    #[test]
    fn refuses_a_host_that_only_overflows_once_the_default_port_is_added() {
        // 61 'a's + ":443" is 65, one past MAX_HOST (64).
        let host = "a".repeat(61);
        let url = format!("https://{host}/teddiebox.txt");
        assert_eq!(split(&url), Err(OtaError::ValueTooLong));
    }

    // A fragment is client-only and must never reach the wire; a query
    // string was never part of this key's contract either. The module doc
    // justifies taking a full URL precisely because it is what someone
    // copies out of a browser bar -- the one place a `#fragment` comes from.
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

    // build_path_request interpolates the resolved path into an HTTP
    // request line unescaped. A space adds a token to that line; this is
    // what an unquoted `image = a.bin HTTP/1.1` in the manifest would do.
    #[test]
    fn resolve_image_refuses_a_space() {
        assert_eq!(
            resolve_image("/content/FIRMWARE/teddiebox.txt", "a.bin HTTP/1.1"),
            Err(OtaError::MalformedUrl)
        );
    }

    // A lone CR is a line terminator to some HTTP parsers and proxies, even
    // without an LF alongside it (str::lines already keeps an LF from
    // getting this far) -- a request-smuggling primitive, not just a typo.
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
}
