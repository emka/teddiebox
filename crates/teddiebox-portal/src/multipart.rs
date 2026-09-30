//! Takes the file out of a `multipart/form-data` upload.
//!
//! Only as much of RFC 7578 as a form with one file input needs: the content
//! of the first part, whatever its name. The file is binary, so it ends only
//! at the whole delimiter, `\r\n--` and the boundary, never at `\r\n--` alone.

use crate::http::find;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultipartError {
    /// Not `multipart/form-data`, or no boundary to split it by.
    NotMultipart,
    /// No complete part in the body.
    NoFile,
    /// A part with nothing in it: the form was sent with no file chosen.
    Empty,
}

/// The first part's content, borrowed from `body`.
pub fn file<'a>(content_type: Option<&str>, body: &'a [u8]) -> Result<&'a [u8], MultipartError> {
    let boundary = boundary(content_type.ok_or(MultipartError::NotMultipart)?)?;

    let open = find_delimiter(body, b"--", boundary).ok_or(MultipartError::NoFile)?;
    let rest = &body[open + 2 + boundary.len()..];
    let start = find(rest, b"\r\n\r\n").ok_or(MultipartError::NoFile)? + 4;
    let content = &rest[start..];
    let end = find_delimiter(content, b"\r\n--", boundary).ok_or(MultipartError::NoFile)?;

    match &content[..end] {
        [] => Err(MultipartError::Empty),
        file => Ok(file),
    }
}

/// Explains a refused upload in words a person can act on.
pub fn describe(trouble: MultipartError) -> &'static str {
    match trouble {
        MultipartError::NotMultipart | MultipartError::NoFile => {
            "that upload did not arrive intact"
        }
        MultipartError::Empty => "no file chosen — pick tcca.der first",
    }
}

/// The boundary parameter of a `multipart/form-data` content type, unquoted.
fn boundary(content_type: &str) -> Result<&[u8], MultipartError> {
    let mut params = content_type.split(';');
    let is_form_data = params
        .next()
        .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("multipart/form-data"));
    if !is_form_data {
        return Err(MultipartError::NotMultipart);
    }
    params
        .find_map(|param| {
            let (name, value) = param.split_once('=')?;
            name.trim()
                .eq_ignore_ascii_case("boundary")
                .then(|| value.trim().trim_matches('"'))
        })
        .filter(|boundary| !boundary.is_empty())
        .map(str::as_bytes)
        .ok_or(MultipartError::NotMultipart)
}

/// Where `lead` followed by `boundary` first starts in `haystack`.
///
/// Two slices rather than one joined needle, so nothing is allocated.
fn find_delimiter(haystack: &[u8], lead: &[u8], boundary: &[u8]) -> Option<usize> {
    let len = lead.len() + boundary.len();
    (0..=haystack.len().checked_sub(len)?).find(|&at| {
        haystack[at..].starts_with(lead) && haystack[at + lead.len()..].starts_with(boundary)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What Chrome sends for `<input type=file name=ca>` with `ca.der`
    /// chosen. The file holds `\r\n--X`, which is not the delimiter.
    const CHROME_TYPE: &str = "multipart/form-data; boundary=----WebKitFormBoundary7MA4YWxkTrZu0gW";
    const CHROME_BODY: &[u8] = b"------WebKitFormBoundary7MA4YWxkTrZu0gW\r\n\
Content-Disposition: form-data; name=\"ca\"; filename=\"ca.der\"\r\n\
Content-Type: application/x-x509-ca-cert\r\n\
\r\n\
\x30\x82\r\n--X\x01\
\r\n------WebKitFormBoundary7MA4YWxkTrZu0gW--\r\n";

    /// What Firefox sends for the same form.
    const FIREFOX_TYPE: &str =
        "multipart/form-data; boundary=---------------------------9051914041544843365972754266";
    const FIREFOX_BODY: &[u8] = b"-----------------------------9051914041544843365972754266\r\n\
Content-Disposition: form-data; name=\"ca\"; filename=\"ca.der\"\r\n\
Content-Type: application/octet-stream\r\n\
\r\n\
\x30\x82\x01\x02\
\r\n-----------------------------9051914041544843365972754266--\r\n";

    #[test]
    fn chromes_upload_yields_the_file() {
        // Given
        let (content_type, body) = (CHROME_TYPE, CHROME_BODY);

        // When
        let got = file(Some(content_type), body);

        // Then
        assert_eq!(got, Ok(&b"\x30\x82\r\n--X\x01"[..]));
    }

    #[test]
    fn firefoxs_upload_yields_the_file() {
        // Given
        let (content_type, body) = (FIREFOX_TYPE, FIREFOX_BODY);

        // When
        let got = file(Some(content_type), body);

        // Then
        assert_eq!(got, Ok(&b"\x30\x82\x01\x02"[..]));
    }

    #[test]
    fn a_crlf_dash_dash_inside_the_file_does_not_end_it() {
        // Given
        let (content_type, body) = (CHROME_TYPE, CHROME_BODY);

        // When
        let got = file(Some(content_type), body).unwrap();

        // Then
        assert_eq!(got.len(), 8);
    }

    #[test]
    fn a_quoted_boundary_is_unquoted() {
        // Given
        let content_type = "multipart/form-data; boundary=\"abc\"";
        let body = b"--abc\r\n\
Content-Disposition: form-data; name=\"ca\"; filename=\"a\"\r\n\
\r\n\
X\r\n--abc--\r\n";

        // When
        let got = file(Some(content_type), body);

        // Then
        assert_eq!(got, Ok(&b"X"[..]));
    }

    #[test]
    fn an_upload_with_no_file_chosen_is_refused() {
        // Given
        let body = b"------WebKitFormBoundary7MA4YWxkTrZu0gW\r\n\
Content-Disposition: form-data; name=\"ca\"; filename=\"\"\r\n\
Content-Type: application/octet-stream\r\n\
\r\n\
\r\n------WebKitFormBoundary7MA4YWxkTrZu0gW--\r\n";

        // When
        let got = file(Some(CHROME_TYPE), body);

        // Then
        assert_eq!(got, Err(MultipartError::Empty));
    }

    #[test]
    fn a_form_post_is_not_an_upload() {
        // Given
        let content_type = "application/x-www-form-urlencoded";

        // When
        let got = file(Some(content_type), b"config=x");

        // Then
        assert_eq!(got, Err(MultipartError::NotMultipart));
    }

    #[test]
    fn a_request_without_a_content_type_is_not_an_upload() {
        // Given
        let content_type = None;

        // When
        let got = file(content_type, CHROME_BODY);

        // Then
        assert_eq!(got, Err(MultipartError::NotMultipart));
    }

    #[test]
    fn multipart_without_a_boundary_is_refused() {
        // Given
        let content_type = "multipart/form-data";

        // When
        let got = file(Some(content_type), CHROME_BODY);

        // Then
        assert_eq!(got, Err(MultipartError::NotMultipart));
    }

    #[test]
    fn a_body_cut_off_before_the_closing_delimiter_is_refused() {
        // Given: the body ends partway through the closing delimiter.
        let cut = &CHROME_BODY[..CHROME_BODY.len() - 10];

        // When
        let got = file(Some(CHROME_TYPE), cut);

        // Then
        assert_eq!(got, Err(MultipartError::NoFile));
    }

    #[test]
    fn a_body_under_a_different_boundary_is_refused() {
        // Given
        let (content_type, body) = (FIREFOX_TYPE, CHROME_BODY);

        // When
        let got = file(Some(content_type), body);

        // Then
        assert_eq!(got, Err(MultipartError::NoFile));
    }
}
