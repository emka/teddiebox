#![no_std]

//! Client for a LAN-local teddyCloud server.

pub mod request;
pub mod response;
pub mod stream;

pub use request::{
    build_content_request, build_length_probe, build_path_request, parse_etag, ContentRequest,
    ETag, Route, MAX_ETAG,
};
pub use response::{parse_head, ContentRange, ResponseHead};
pub use stream::{begin, begin_prepared, probe_length, Begun, Body, Probed};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudError {
    RequestTooLong,
    Transport,
    MalformedResponse,
    UnexpectedStatus(u16),
    /// The peer closed before delivering the body it promised.
    BodyTruncated,
    /// The response will not fit the caller's buffer.
    ResponseTooLong,
    /// A response with content has no `Content-Length`, so the body's end is
    /// unknown. A keep-alive server never closes the connection to mark it.
    LengthRequired,
    /// The body is chunked, and nothing here removes the chunk framing.
    /// Without this error, chunk sizes would reach the Opus decoder as audio.
    UnsupportedTransferEncoding,
}

impl CloudError {
    /// Whether the server answered, and its answer means "nothing is filed
    /// under that identifier" rather than a fault worth retrying.
    ///
    /// teddyCloud answers `403` to a request without a usable token, and
    /// `404` to one it has no story for. Every other status, and every other
    /// kind of failure (a socket error, a malformed response), leaves the
    /// question open, so it is *not* treated as "no content": wrongly
    /// telling someone the network is fine when it is not is the worse
    /// mistake.
    pub fn means_no_content(&self) -> bool {
        matches!(self, CloudError::UnexpectedStatus(403 | 404))
    }
}

#[cfg(test)]
mod tests {
    use super::CloudError;

    #[test]
    fn a_403_means_no_content() {
        assert!(CloudError::UnexpectedStatus(403).means_no_content());
    }

    #[test]
    fn a_404_means_no_content() {
        assert!(CloudError::UnexpectedStatus(404).means_no_content());
    }

    #[test]
    fn a_500_does_not_mean_no_content() {
        assert!(!CloudError::UnexpectedStatus(500).means_no_content());
    }

    #[test]
    fn a_transport_failure_does_not_mean_no_content() {
        assert!(!CloudError::Transport.means_no_content());
    }
}
