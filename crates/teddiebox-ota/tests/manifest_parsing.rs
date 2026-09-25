//! Every way a manifest can be wrong.
//!
//! The manifest comes from the network and decides what gets flashed, so the
//! parser refuses anything unclear rather than guessing.

use teddiebox_ota::{Manifest, OtaError, MAX_MANIFEST};

fn parse(s: &str) -> Result<Manifest, OtaError> {
    Manifest::parse_read(s.as_bytes(), MAX_MANIFEST)
}

const DIGEST: &str = "3f786850e387550fdab836ed7e6dc881de23001b000000000000000000000000";

#[test]
fn a_missing_version_is_refused() {
    let s = format!("sha256 = {DIGEST}\nlength = 1103728\nimage = teddiebox.bin\n");
    assert_eq!(parse(&s), Err(OtaError::MissingVersion));
}

#[test]
fn an_empty_version_is_refused() {
    let s = format!("version = \nsha256 = {DIGEST}\nlength = 1\nimage = a.bin\n");
    assert_eq!(parse(&s), Err(OtaError::MissingVersion));
}

#[test]
fn a_whitespace_only_version_is_refused() {
    let s = format!("version =    \nsha256 = {DIGEST}\nlength = 1\nimage = a.bin\n");
    assert_eq!(parse(&s), Err(OtaError::MissingVersion));
}

#[test]
fn a_missing_digest_is_refused() {
    let s = "version = v1\nlength = 1103728\nimage = teddiebox.bin\n";
    assert_eq!(parse(s), Err(OtaError::MissingSha256));
}

#[test]
fn a_missing_length_is_refused() {
    let s = format!("version = v1\nsha256 = {DIGEST}\nimage = teddiebox.bin\n");
    assert_eq!(parse(&s), Err(OtaError::MissingLength));
}

#[test]
fn a_missing_image_is_refused() {
    let s = format!("version = v1\nsha256 = {DIGEST}\nlength = 1103728\n");
    assert_eq!(parse(&s), Err(OtaError::MissingImage));
}

#[test]
fn an_empty_image_is_refused() {
    let s = format!("version = v1\nsha256 = {DIGEST}\nlength = 1\nimage = \n");
    assert_eq!(parse(&s), Err(OtaError::MissingImage));
}

#[test]
fn a_whitespace_only_image_is_refused() {
    let s = format!("version = v1\nsha256 = {DIGEST}\nlength = 1\nimage =    \n");
    assert_eq!(parse(&s), Err(OtaError::MissingImage));
}

#[test]
fn a_line_without_an_equals_sign_is_refused() {
    let s =
        format!("version = v1\nthis is not a setting\nsha256 = {DIGEST}\nlength = 1\nimage = a\n");
    assert_eq!(parse(&s), Err(OtaError::MalformedLine));
}

#[test]
fn a_non_numeric_length_is_refused() {
    let s = format!("version = v1\nsha256 = {DIGEST}\nlength = lots\nimage = a\n");
    assert_eq!(parse(&s), Err(OtaError::MalformedLength));
}

#[test]
fn a_negative_length_is_refused() {
    let s = format!("version = v1\nsha256 = {DIGEST}\nlength = -1\nimage = a\n");
    assert_eq!(parse(&s), Err(OtaError::MalformedLength));
}

#[test]
fn a_length_beyond_u32_is_refused() {
    let s = format!("version = v1\nsha256 = {DIGEST}\nlength = 4294967296\nimage = a\n");
    assert_eq!(parse(&s), Err(OtaError::MalformedLength));
}

#[test]
fn a_bad_digest_is_refused() {
    let s = "version = v1\nsha256 = deadbeef\nlength = 1\nimage = a\n";
    assert_eq!(parse(s), Err(OtaError::MalformedDigest));
}

#[test]
fn a_version_longer_than_the_buffer_is_refused() {
    // MAX_VERSION is 31 (the app descriptor's version field is 32 bytes,
    // less a NUL), so 32 characters is one over.
    let long = "v".repeat(32);
    let s = format!("version = {long}\nsha256 = {DIGEST}\nlength = 1\nimage = a\n");
    assert_eq!(parse(&s), Err(OtaError::ValueTooLong));
}

#[test]
fn an_image_path_longer_than_the_buffer_is_refused() {
    let long = "i".repeat(65);
    let s = format!("version = v1\nsha256 = {DIGEST}\nlength = 1\nimage = {long}\n");
    assert_eq!(parse(&s), Err(OtaError::ValueTooLong));
}

#[test]
fn bytes_that_are_not_utf8_are_refused() {
    assert_eq!(
        Manifest::parse_read(&[0xff, 0xfe, 0x00], MAX_MANIFEST),
        Err(OtaError::NotText)
    );
}

#[test]
fn a_read_that_filled_its_buffer_is_refused_as_truncated() {
    let s = format!("version = v1\nsha256 = {DIGEST}\nlength = 1\nimage = a\n");
    let capacity = s.len();
    assert_eq!(
        Manifest::parse_read(s.as_bytes(), capacity),
        Err(OtaError::Truncated)
    );
}

#[test]
fn crlf_line_endings_parse() {
    let s = format!("version = v1\r\nsha256 = {DIGEST}\r\nlength = 1103728\r\nimage = a.bin\r\n");
    let m = parse(&s).unwrap();
    assert_eq!(m.version.as_str(), "v1");
    assert_eq!(m.length, 1_103_728);
}

#[test]
fn a_file_with_no_trailing_newline_parses() {
    let s = format!("version = v1\nsha256 = {DIGEST}\nlength = 1103728\nimage = a.bin");
    assert_eq!(parse(&s).unwrap().length, 1_103_728);
}

#[test]
fn blank_lines_and_comments_are_ignored() {
    let s = format!(
        "# published 2026-09-15\n\nversion = v1\n\nsha256 = {DIGEST}\nlength = 7\nimage = a.bin\n"
    );
    assert_eq!(parse(&s).unwrap().length, 7);
}

#[test]
fn an_unknown_key_is_ignored_so_an_older_box_reads_a_newer_manifest() {
    let s = format!(
        "version = v1\nsignature = whatever\nsha256 = {DIGEST}\nlength = 7\nimage = a.bin\n"
    );
    assert_eq!(parse(&s).unwrap().version.as_str(), "v1");
}

#[test]
fn the_last_of_a_duplicated_key_wins() {
    let s = format!("version = v1\nversion = v2\nsha256 = {DIGEST}\nlength = 7\nimage = a.bin\n");
    assert_eq!(parse(&s).unwrap().version.as_str(), "v2");
}
