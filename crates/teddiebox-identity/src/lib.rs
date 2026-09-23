#![no_std]
//! The `cert` partition's layout: a versioned header, then two DER bodies.
//!
//! One crate for both sides. `tools/identity-image` renders the image on a
//! laptop and the firmware parses it on the box, so a change to the layout
//! that only one side follows fails `cargo test` rather than a bench session.
//!
//! **Read in two steps, not one.** [`parse_header`] takes only the first
//! [`HEADER`] bytes and reports where the bodies are, so the firmware never
//! needs the whole image in one buffer — the partition is 16 KB and the box's
//! stack is not.

/// Marks the partition as holding an identity this firmware wrote.
pub const MAGIC: [u8; 4] = *b"TBID";

/// The layout's version.
///
/// Bumped when the header or the body order changes. A firmware that does not
/// know a version refuses the image rather than guessing at it — the whole
/// reason the field is here.
pub const VERSION: u16 = 1;

/// Bytes before the first body.
pub const HEADER: usize = 16;

/// The longest certificate or key the box can hold.
///
/// Must equal `tls::CERT_BYTES` in the firmware, which asserts it. A length
/// this side accepted and that side could not hold would be a refusal at the
/// worst possible moment — on the box, after provisioning looked fine.
pub const MAX_BODY: usize = 1536;

/// What went wrong with an image, in terms the box can say out loud.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityError {
    /// Erased flash. Not a fault: it is what an unprovisioned box looks like,
    /// and it is worth its own variant so the box can say "run `just identity`"
    /// rather than "this partition is corrupt".
    Blank,
    /// Something is there, and it is not ours.
    Magic,
    /// Ours, from a firmware that wrote a layout this one does not know.
    Version,
    /// A length of zero. A key that is not there is not a key.
    Empty,
    /// A body longer than the buffers it has to land in.
    TooLong,
    /// The image claims more than the partition or the buffer holds.
    Truncated,
}

/// Where the bodies are, once the header has been believed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub certificate_len: usize,
    pub key_len: usize,
}

impl Header {
    pub const fn certificate_offset(&self) -> usize {
        HEADER
    }

    pub const fn key_offset(&self) -> usize {
        HEADER + self.certificate_len
    }

    pub const fn total_len(&self) -> usize {
        HEADER + self.certificate_len + self.key_len
    }
}

/// Reads the header and nothing else.
///
/// `capacity` is how many bytes are actually available — the partition's
/// length on the box, the buffer's length on a laptop. Checking the claim
/// against it here is what stops a later read running off the end.
pub fn parse_header(raw: &[u8], capacity: usize) -> Result<Header, IdentityError> {
    if raw.len() < HEADER {
        return Err(IdentityError::Truncated);
    }
    let header = &raw[..HEADER];

    // Erased flash first, because it would otherwise be reported as a wrong
    // magic — true, and useless to whoever has simply not provisioned the box.
    if header.iter().all(|&byte| byte == 0xFF) {
        return Err(IdentityError::Blank);
    }
    if header[..4] != MAGIC[..] {
        return Err(IdentityError::Magic);
    }
    if u16::from_le_bytes([header[4], header[5]]) != VERSION {
        return Err(IdentityError::Version);
    }

    let certificate_len = u16::from_le_bytes([header[6], header[7]]) as usize;
    let key_len = u16::from_le_bytes([header[8], header[9]]) as usize;
    if certificate_len == 0 || key_len == 0 {
        return Err(IdentityError::Empty);
    }
    if certificate_len > MAX_BODY || key_len > MAX_BODY {
        return Err(IdentityError::TooLong);
    }

    let held = Header {
        certificate_len,
        key_len,
    };
    if held.total_len() > capacity {
        return Err(IdentityError::Truncated);
    }
    Ok(held)
}

/// Writes the whole image into `out`, returning how much of it was used.
///
/// The reserved bytes are zeroed rather than left as they were found: an
/// image built twice from the same inputs must be the same bytes, or the
/// fixture that pins this layout would pass on one machine and fail on
/// another.
pub fn render(certificate: &[u8], key: &[u8], out: &mut [u8]) -> Result<usize, IdentityError> {
    if certificate.is_empty() || key.is_empty() {
        return Err(IdentityError::Empty);
    }
    if certificate.len() > MAX_BODY || key.len() > MAX_BODY {
        return Err(IdentityError::TooLong);
    }
    let total = HEADER + certificate.len() + key.len();
    if total > out.len() {
        return Err(IdentityError::Truncated);
    }

    out[..4].copy_from_slice(&MAGIC);
    out[4..6].copy_from_slice(&VERSION.to_le_bytes());
    out[6..8].copy_from_slice(&(certificate.len() as u16).to_le_bytes());
    out[8..10].copy_from_slice(&(key.len() as u16).to_le_bytes());
    out[10..HEADER].fill(0);
    out[HEADER..HEADER + certificate.len()].copy_from_slice(certificate);
    out[HEADER + certificate.len()..total].copy_from_slice(key);
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stand-ins for the real DER. Nothing here is a key, and nothing here
    /// should ever be one: this file is committed.
    const CERTIFICATE: &[u8] = b"not-a-certificate";
    const KEY: &[u8] = b"not-a-key";

    /// The point of one crate owning both directions: what the tool writes is
    /// what the box reads, and this is the test that says so.
    #[test]
    fn a_rendered_image_parses_back_to_what_it_held() {
        let mut image = [0xFFu8; 64];
        let len = render(CERTIFICATE, KEY, &mut image).unwrap();
        assert_eq!(len, HEADER + CERTIFICATE.len() + KEY.len());

        let header = parse_header(&image, image.len()).unwrap();
        assert_eq!(header.certificate_len, CERTIFICATE.len());
        assert_eq!(header.key_len, KEY.len());
        assert_eq!(
            &image
                [header.certificate_offset()..header.certificate_offset() + header.certificate_len],
            CERTIFICATE
        );
        assert_eq!(
            &image[header.key_offset()..header.key_offset() + header.key_len],
            KEY
        );
    }

    /// An unprovisioned box, which is an ordinary state and not a fault.
    #[test]
    fn erased_flash_is_blank_rather_than_malformed() {
        let image = [0xFFu8; 64];
        assert_eq!(parse_header(&image, image.len()), Err(IdentityError::Blank));
    }

    #[test]
    fn something_that_is_not_ours_is_refused_by_its_magic() {
        let mut image = [0u8; 64];
        let len = render(CERTIFICATE, KEY, &mut image).unwrap();
        image[0] = b'X';
        assert_eq!(parse_header(&image[..len], len), Err(IdentityError::Magic));
    }

    /// The field exists so that a layout change is refused rather than
    /// misread. A test that did not check this would leave it decorative.
    #[test]
    fn a_version_this_firmware_does_not_know_is_refused() {
        let mut image = [0u8; 64];
        let len = render(CERTIFICATE, KEY, &mut image).unwrap();
        image[4..6].copy_from_slice(&(VERSION + 1).to_le_bytes());
        assert_eq!(
            parse_header(&image[..len], len),
            Err(IdentityError::Version)
        );
    }

    #[test]
    fn a_body_longer_than_the_buffers_is_refused() {
        let mut image = [0u8; 64];
        let len = render(CERTIFICATE, KEY, &mut image).unwrap();
        image[6..8].copy_from_slice(&((MAX_BODY + 1) as u16).to_le_bytes());
        assert_eq!(
            parse_header(&image[..len], len),
            Err(IdentityError::TooLong)
        );
    }

    /// The check that stops a body read running off the end of the partition.
    #[test]
    fn an_image_claiming_more_than_it_has_is_refused() {
        let mut image = [0u8; 64];
        let len = render(CERTIFICATE, KEY, &mut image).unwrap();
        assert_eq!(
            parse_header(&image[..len], len - 1),
            Err(IdentityError::Truncated)
        );
    }

    /// The other tests build input with `render` and read it back with
    /// `parse_header` — both defined in this file, so a layout bug that
    /// moves a field the same way in both (a reviewer once swapped
    /// `certificate_len` and `key_len` in both functions) passes every one
    /// of them. This pins the actual bytes `render` writes, hand-written
    /// rather than assembled from the code's own field constants, so the
    /// layout cannot move without this failing for the right reason.
    #[test]
    fn a_rendered_image_matches_the_documented_byte_layout() {
        let mut image = [0u8; 64];
        let len = render(CERTIFICATE, KEY, &mut image).unwrap();

        #[rustfmt::skip]
        let expected: [u8; 42] = [
            // 0x00-0x03: magic "TBID"
            0x54, 0x42, 0x49, 0x44,
            // 0x04-0x05: version 1, little-endian
            0x01, 0x00,
            // 0x06-0x07: certificate length 17, little-endian
            0x11, 0x00,
            // 0x08-0x09: key length 9, little-endian
            0x09, 0x00,
            // 0x0a-0x0f: reserved
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            // certificate: "not-a-certificate"
            b'n', b'o', b't', b'-', b'a', b'-', b'c', b'e', b'r', b't', b'i',
            b'f', b'i', b'c', b'a', b't', b'e',
            // key: "not-a-key"
            b'n', b'o', b't', b'-', b'a', b'-', b'k', b'e', b'y',
        ];

        assert_eq!(len, expected.len());
        assert_eq!(&image[..len], &expected[..]);
    }

    #[test]
    fn a_zero_length_body_is_refused() {
        let mut image = [0u8; 64];
        let len = render(CERTIFICATE, KEY, &mut image).unwrap();
        image[8..10].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(parse_header(&image[..len], len), Err(IdentityError::Empty));
    }

    #[test]
    fn a_header_that_did_not_all_arrive_is_refused() {
        let mut image = [0u8; 64];
        render(CERTIFICATE, KEY, &mut image).unwrap();
        assert_eq!(
            parse_header(&image[..HEADER - 1], HEADER - 1),
            Err(IdentityError::Truncated)
        );
    }

    #[test]
    fn rendering_refuses_a_buffer_it_would_overrun() {
        let mut image = [0u8; HEADER + 4];
        assert_eq!(
            render(CERTIFICATE, KEY, &mut image),
            Err(IdentityError::Truncated)
        );
    }
}
