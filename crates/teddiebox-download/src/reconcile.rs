//! Turns the server's answer into what the writer should do.
//!
//! [`crate::decide`] decides what to ask for; `teddiebox_cloud::stream::begin`
//! reports what came back. This checks one against the other. For example, a
//! `Resume` can be answered with a `200` from byte zero (the server ignored
//! the range). Appending that body would put the start of the story after
//! its middle, and the file would then look complete and never be fixed.

use crate::Decision;
use teddiebox_cloud::{Begun, ETag};

/// What to do about a response, once it has been checked against what was
/// asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Append the body to what is already on the card. Start the writer at
    /// `resume_from`; the existing sidecar is still valid.
    Append { resume_from: u32, total: u32 },
    /// Truncate whatever is on the card and write from zero, then rewrite the
    /// sidecar with these values.
    Restart { total: u32, etag: Option<ETag> },
    /// The server has nothing for this tag.
    Absent,
    /// Write nothing, and say why.
    Refuse(Mismatch),
}

/// Why [`reconcile`] refused to turn a response into bytes on the card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mismatch {
    /// A body offered at an offset we did not ask for.
    WrongOffset { asked: u32, got: u32 },
    /// The server did not give the file's length, so the download could
    /// never be known to be complete.
    LengthUnknown,
    /// A 304. Only meaningful for revalidation, not for a download.
    NotModified,
    /// The decision was to play what is on the card, so no request should
    /// have been made.
    NothingRequested,
    /// The whole file is shorter than what the card already holds.
    ShorterThanWhatIsOnTheCard { total: u32, on_card: u32 },
}

/// Checks a [`Decision`] against the [`Begun`] the request got back, and
/// returns what to do.
pub fn reconcile(decision: &Decision, begun: &Begun) -> Action {
    // With `Play`, no request should have been sent, so refuse before
    // looking at the response.
    let requested = match decision {
        Decision::Play => return Action::Refuse(Mismatch::NothingRequested),
        Decision::Fetch => None,
        Decision::Resume { from, etag } => Some((*from, etag)),
    };

    let (offset, total, response_etag) = match begun {
        Begun::NotFound => return Action::Absent,
        Begun::Unchanged => return Action::Refuse(Mismatch::NotModified),
        // Without a length the download can never be known complete.
        Begun::Content { total: None, .. } => return Action::Refuse(Mismatch::LengthUnknown),
        Begun::Content {
            offset,
            total: Some(total),
            etag,
            ..
        } => (*offset, *total, etag),
    };

    match requested {
        None => {
            if offset == 0 {
                Action::Restart {
                    total,
                    etag: response_etag.clone(),
                }
            } else {
                Action::Refuse(Mismatch::WrongOffset {
                    asked: 0,
                    got: offset,
                })
            }
        }
        Some((from, requested_etag)) => {
            if offset == from {
                // A total shorter than the offset is impossible from a
                // correct server. Refuse rather than resume past the end of
                // the file.
                if total < from {
                    return Action::Refuse(Mismatch::ShorterThanWhatIsOnTheCard {
                        total,
                        on_card: from,
                    });
                }

                // With `If-Range`, a `206` at our offset already means the
                // etag matched. This check only catches a server that
                // ignores `If-Range`, but it is cheap.
                if let (Some(requested_etag), Some(response_etag)) = (requested_etag, response_etag)
                {
                    if requested_etag != response_etag {
                        return Action::Restart {
                            total,
                            etag: Some(response_etag.clone()),
                        };
                    }
                }

                // This includes `from == 0` answered at `offset == 0`: the
                // normal state right after the sidecar was written, before
                // any body byte arrived.
                Action::Append {
                    resume_from: from,
                    total,
                }
            } else if offset == 0 {
                // Here `from != 0`: the server ignored the range and sent
                // the whole file, so start the file again.
                Action::Restart {
                    total,
                    etag: response_etag.clone(),
                }
            } else {
                Action::Refuse(Mismatch::WrongOffset {
                    asked: from,
                    got: offset,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::ops::Range;

    fn content(offset: u32, total: Option<u32>, etag: Option<ETag>) -> Begun {
        Begun::Content {
            etag,
            body_length: 0,
            offset,
            total,
            prefix: Range { start: 0, end: 0 },
        }
    }

    fn etag(s: &str) -> Option<ETag> {
        ETag::try_from(s).ok()
    }

    #[test]
    fn a_resume_answered_at_the_offset_it_asked_for_appends() {
        assert_eq!(
            reconcile(
                &Decision::Resume {
                    from: 100,
                    etag: etag("\"v1\""),
                },
                &content(100, Some(5000), etag("\"v1\"")),
            ),
            Action::Append {
                resume_from: 100,
                total: 5000,
            }
        );
    }

    /// Resuming from zero is normal right after the sidecar was written. It
    /// must not be mistaken for a server ignoring the range.
    #[test]
    fn resuming_from_zero_answered_at_zero_appends_rather_than_restarts() {
        assert_eq!(
            reconcile(
                &Decision::Resume {
                    from: 0,
                    etag: None
                },
                &content(0, Some(5000), None),
            ),
            Action::Append {
                resume_from: 0,
                total: 5000,
            }
        );
    }

    /// A `200` to a range request means the server sent the whole file, so
    /// the partial file is thrown away.
    #[test]
    fn a_resume_declined_and_answered_from_the_start_restarts() {
        assert_eq!(
            reconcile(
                &Decision::Resume {
                    from: 100,
                    etag: etag("\"v1\""),
                },
                &content(0, Some(5000), etag("\"v2\"")),
            ),
            Action::Restart {
                total: 5000,
                etag: etag("\"v2\""),
            }
        );
    }

    #[test]
    fn a_resume_answered_at_an_unexpected_offset_is_refused() {
        assert_eq!(
            reconcile(
                &Decision::Resume {
                    from: 100,
                    etag: None
                },
                &content(50, Some(5000), None),
            ),
            Action::Refuse(Mismatch::WrongOffset {
                asked: 100,
                got: 50
            })
        );
    }

    /// The server continues from byte 100 but says the file is 50 bytes long,
    /// which is impossible.
    #[test]
    fn a_resume_answered_with_a_total_shorter_than_what_is_on_the_card_is_refused() {
        assert_eq!(
            reconcile(
                &Decision::Resume {
                    from: 100,
                    etag: None
                },
                &content(100, Some(50), None),
            ),
            Action::Refuse(Mismatch::ShorterThanWhatIsOnTheCard {
                total: 50,
                on_card: 100,
            })
        );
    }

    #[test]
    fn a_fetch_answered_from_the_start_restarts() {
        assert_eq!(
            reconcile(&Decision::Fetch, &content(0, Some(5000), etag("\"v1\""))),
            Action::Restart {
                total: 5000,
                etag: etag("\"v1\""),
            }
        );
    }

    #[test]
    fn a_fetch_answered_mid_file_is_refused() {
        assert_eq!(
            reconcile(&Decision::Fetch, &content(50, Some(5000), None)),
            Action::Refuse(Mismatch::WrongOffset { asked: 0, got: 50 })
        );
    }

    /// Without a length the download could never be known complete. Checked
    /// before the offset.
    #[test]
    fn a_response_with_no_declared_length_is_refused_before_any_offset_check() {
        assert_eq!(
            reconcile(
                &Decision::Resume {
                    from: 100,
                    etag: None
                },
                &content(100, None, None),
            ),
            Action::Refuse(Mismatch::LengthUnknown)
        );
    }

    #[test]
    fn no_content_for_the_tag_is_reported_as_absent() {
        assert_eq!(
            reconcile(&Decision::Fetch, &Begun::NotFound),
            Action::Absent
        );
    }

    #[test]
    fn a_304_is_refused_because_it_never_means_anything_for_a_download() {
        assert_eq!(
            reconcile(
                &Decision::Resume {
                    from: 100,
                    etag: None
                },
                &Begun::Unchanged
            ),
            Action::Refuse(Mismatch::NotModified)
        );
    }

    /// With `Play`, the file on the card is already complete and no request
    /// should have been sent, so any response is refused.
    #[test]
    fn playing_what_is_on_the_card_refuses_any_response_regardless_of_its_content() {
        assert_eq!(
            reconcile(&Decision::Play, &content(0, Some(5000), etag("\"v1\""))),
            Action::Refuse(Mismatch::NothingRequested)
        );
    }

    /// With `If-Range`, this should not happen. The check catches a server
    /// that ignores `If-Range`, which would otherwise join two versions of a
    /// file.
    #[test]
    fn an_append_whose_etag_disagrees_with_the_one_asked_for_restarts_instead() {
        assert_eq!(
            reconcile(
                &Decision::Resume {
                    from: 100,
                    etag: etag("\"v1\""),
                },
                &content(100, Some(5000), etag("\"v2\"")),
            ),
            Action::Restart {
                total: 5000,
                etag: etag("\"v2\""),
            }
        );
    }
}
