//! Turning what the server actually answered into what the writer should do.
//!
//! [`crate::decide`] decides what to ask for; `teddiebox_cloud::stream::begin`
//! reports what came back. The two were built without knowledge of each
//! other, and the gap between them is exactly where a partial file gets
//! corrupted: a `Resume` can be answered with a `200` from byte zero — the
//! server declining the range — and a caller that blindly appends that body
//! to what is already on the card splices the beginning of the story into
//! its middle. The file then looks complete on the card and is never
//! refetched. Every branch here is a case the box can hit for real; stating
//! them as one pure function, tested by table, is what keeps a server's
//! answer from turning into a story that jumps.

use crate::Decision;
use teddiebox_cloud::{Begun, ETag};

/// What to do about a response, once it has been checked against what was
/// asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Append the body to what is already on the card. Seed the writer at
    /// `resume_from`; the sidecar already there still stands.
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
    /// The server did not say how long the whole file is, so no sidecar could
    /// vouch for it and the download could never be known complete.
    LengthUnknown,
    /// A 304. Meaningful for revalidation, never for a download.
    NotModified,
    /// The decision was to play what is on the card; no request should have
    /// been made at all.
    NothingRequested,
    /// The whole file is shorter than what the card already holds.
    ShorterThanWhatIsOnTheCard { total: u32, on_card: u32 },
}

/// Reconciles a [`Decision`] against the [`Begun`] the request actually got
/// back, so that a caller can act on the pair without re-deriving any of the
/// checks below itself.
pub fn reconcile(decision: &Decision, begun: &Begun) -> Action {
    // A `Play` decision means no request should have gone out at all, so
    // whatever the server said is moot — check this, like the length below,
    // before doing any arithmetic on a response about to be refused anyway.
    let requested = match decision {
        Decision::Play => return Action::Refuse(Mismatch::NothingRequested),
        Decision::Fetch => None,
        Decision::Resume { from, etag } => Some((*from, etag)),
    };

    let (offset, total, response_etag) = match begun {
        Begun::NotFound => return Action::Absent,
        Begun::Unchanged => return Action::Refuse(Mismatch::NotModified),
        // No declared length means no sidecar could ever vouch for the
        // result, so there is nothing to compare `from` against below.
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
                // The offset we asked for coming back paired with a total
                // shorter than it is a contradiction no correct server
                // produces — continuing from byte `from` implies a file at
                // least that long. Refuse rather than hand the writer a
                // resume point past the end of the file it is about to
                // write.
                if total < from {
                    return Action::Refuse(Mismatch::ShorterThanWhatIsOnTheCard {
                        total,
                        on_card: from,
                    });
                }

                // Deliberately redundant: `If-Range` semantics mean a `206`
                // at the offset we asked for already implies the validator
                // matched, so this only ever catches a server that ignores
                // `If-Range` and serves a stale range anyway. The check
                // costs one comparison; missing what it catches costs a
                // spliced file.
                if let (Some(requested_etag), Some(response_etag)) = (requested_etag, response_etag)
                {
                    if requested_etag != response_etag {
                        return Action::Restart {
                            total,
                            etag: Some(response_etag.clone()),
                        };
                    }
                }

                // This also covers `from == 0` answered at `offset == 0`:
                // the ordinary state right after the sidecar was written but
                // before any body byte landed. That is not the
                // range-declined case below, which needs `from` to be
                // nonzero to mean anything.
                Action::Append {
                    resume_from: from,
                    total,
                }
            } else if offset == 0 {
                // `offset == from` was just ruled out, and `offset == 0`
                // here, so `from != 0`: the server declined the range and
                // sent the whole file back from the start. Appending this
                // body would splice the beginning of the story into the
                // middle of what is already on the card.
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

    /// Resuming from zero is the ordinary state right after the sidecar was
    /// written but before any body byte landed. Without this case, the same
    /// `offset == 0` the declined-range check below looks for would be
    /// mistaken for one, and a download that had not lost a single byte
    /// would be restarted from scratch for no reason.
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

    /// A `200` answering a range request is the server declining it and
    /// sending the file from the start. Treating that body as a continuation
    /// would splice the beginning of the story into the middle of what is
    /// already on the card; the fix is to throw the partial file away.
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

    /// The server reported continuing from byte 100 while claiming the whole
    /// file is only 50 bytes long — a contradiction. Trusting `total` here
    /// would hand the writer a resume point past the end of the file it is
    /// about to write.
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

    /// A body whose length the server never stated is one no sidecar could
    /// ever vouch for. Checked before the offset arithmetic, or a response
    /// already destined to be refused would first be compared against
    /// `from` for no reason.
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

    /// `Play` means the file on the card is already complete and vouched
    /// for, so no request should have been sent. Even a response that looks
    /// perfectly good is refused, because there was nothing to ask the
    /// server in the first place.
    #[test]
    fn playing_what_is_on_the_card_refuses_any_response_regardless_of_its_content() {
        assert_eq!(
            reconcile(&Decision::Play, &content(0, Some(5000), etag("\"v1\""))),
            Action::Refuse(Mismatch::NothingRequested)
        );
    }

    /// `If-Range` semantics mean a `206` at the offset asked for already
    /// implies the validator matched, so this check is redundant with the
    /// protocol. It stays because the one server that ignores `If-Range` and
    /// serves a stale range anyway would otherwise splice two versions of a
    /// file together, and the check costs one comparison.
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
