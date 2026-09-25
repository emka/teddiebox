//! What to do about a file that may or may not already be on the card.
//!
//! A download can be interrupted by a lifted figure, a flat battery, or a
//! crash between the two writes that create a cache entry. The rules for each
//! case are pure functions, so they can all be tested on the host.

use crate::Sidecar;
use teddiebox_cloud::ETag;

/// What was found in the cache for one tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cached {
    /// The parsed `.MET` beside the content, if it was there and readable.
    pub sidecar: Option<Sidecar>,
    /// The content file's length on the card, if the file exists at all.
    pub length_on_card: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Complete and vouched for. Play it as it is.
    Play,
    /// Start again from the beginning, truncating anything already there.
    Fetch,
    /// Continue an interrupted download.
    Resume { from: u32, etag: Option<ETag> },
}

pub fn decide(cached: &Cached) -> Decision {
    // Without a sidecar the file's length is unknown, so it may be
    // incomplete.
    let (Some(sidecar), Some(on_card)) = (&cached.sidecar, cached.length_on_card) else {
        return Decision::Fetch;
    };

    match on_card.cmp(&sidecar.length) {
        core::cmp::Ordering::Equal => Decision::Play,
        core::cmp::Ordering::Less => Decision::Resume {
            from: on_card,
            etag: sidecar.etag.clone(),
        },
        // Longer than expected cannot come from a correct download, so start
        // again.
        core::cmp::Ordering::Greater => Decision::Fetch,
    }
}

/// Whether what reached the card adds up to the whole file.
///
/// `from` is where this body was written, `written` how much of it landed, and
/// `total` what the server said the whole file is.
///
/// A download that stopped is not necessarily complete: it may have stopped
/// because the figure was lifted or the connection dropped. Only the byte
/// count tells.
///
/// A file whose length the server never gave is never whole.
pub fn is_whole(from: u32, written: u32, total: Option<u32>) -> bool {
    match total {
        Some(total) => total != 0 && from.saturating_add(written) == total,
        None => false,
    }
}

/// Where a response body belongs on the card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Throw away whatever is there and write from the start.
    Restart,
    /// Continue the file that is already there.
    Continue,
    /// These bytes belong somewhere this file cannot take them.
    Refuse,
}

/// What is known about a body when its response head arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Landing {
    /// Where the server says this body starts in the file.
    pub offset: u32,
    /// How long the content file was when the download was planned.
    pub length_on_card: u32,
    /// What the sidecar promised the whole file would be, if there was one.
    pub expected: Option<u32>,
    /// What the server now says the whole file is, if it said at all.
    pub total: Option<u32>,
}

/// Decides where a body goes, or that it cannot go anywhere.
///
/// Three things can go wrong:
///
/// - The server **ignores the range** and sends the whole file with `200`
///   (`offset: 0`). Appending it would put the start of the story after its
///   middle, so the file is restarted.
/// - The server **sends the requested range of a file that has changed**. The
///   result would be the end of one file joined to the start of another, at
///   exactly the expected length, so a length check could not catch it. So
///   the server's total is compared with the sidecar's: if they differ, the
///   body is refused (costing a new download).
/// - The server **sends bytes at the wrong offset**, leaving a gap or an
///   overlap. Also refused.
///
/// If the server sends no total, the file is assumed unchanged; otherwise
/// resuming would never work with such a server.
pub fn place(landing: &Landing) -> Placement {
    // The body is the whole file, so it replaces what is there whether or
    // not the file changed.
    if landing.offset == 0 {
        return Placement::Restart;
    }

    if let (Some(expected), Some(total)) = (landing.expected, landing.total) {
        if expected != total {
            return Placement::Refuse;
        }
    }

    if landing.offset == landing.length_on_card {
        Placement::Continue
    } else {
        Placement::Refuse
    }
}

/// Whether a complete cached file still matches what the server holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Same length. What is on the card stands.
    Fresh,
    /// A different length. The cached file is not what the server has.
    Stale,
    /// The server said nothing about length, so nothing follows.
    Unknown,
}

/// Compares a complete cached file against what the server currently reports.
///
/// **This is a weak check.** teddyCloud sends **no `ETag`**, `Last-Modified`
/// or `Cache-Control` on its content routes, so the only thing to compare is
/// the length. A change that keeps the same length is not detected. For
/// story audio, which rarely changes, that is acceptable.
///
/// **No answer keeps the file.** Without a length from the server there is no
/// reason to throw away a good file.
///
/// This does not fetch anything; the caller decides whether to ask the
/// server.
pub fn revalidate(sidecar: &Sidecar, server_length: Option<u32>) -> Freshness {
    match server_length {
        None => Freshness::Unknown,
        Some(length) if length == sidecar.length => Freshness::Fresh,
        Some(_) => Freshness::Stale,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A download that stops early still leaves a file on the card; only a
    /// file that reached the server's length is whole.
    #[test]
    fn a_body_that_reaches_the_servers_length_is_whole() {
        assert!(is_whole(0, 38_349_983, Some(38_349_983)));
    }

    #[test]
    fn a_body_that_stops_early_is_not_whole() {
        assert!(!is_whole(0, 1_089_536, Some(38_349_983)));
    }

    /// After an interruption, the resumed body finishes the file.
    #[test]
    fn a_resumed_body_that_finishes_the_file_is_whole() {
        assert!(is_whole(3_145_728, 35_204_255, Some(38_349_983)));
    }

    /// Without a length from the server, nothing can be called whole.
    #[test]
    fn without_a_length_from_the_server_nothing_is_whole() {
        assert!(!is_whole(0, 38_349_983, None));
    }

    /// More bytes than the file has cannot be a correct download of it.
    #[test]
    fn a_body_that_overshoots_is_not_whole() {
        assert!(!is_whole(0, 38_349_984, Some(38_349_983)));
    }
    use teddiebox_cloud::ETag;

    /// A normal resume: half of an 8192-byte file is on the card, and the
    /// server sends the rest of the same file.
    fn agreeing() -> Landing {
        Landing {
            offset: 4096,
            length_on_card: 4096,
            expected: Some(8192),
            total: Some(8192),
        }
    }

    #[test]
    fn a_body_that_starts_at_zero_replaces_what_is_there() {
        let declined = Landing {
            offset: 0,
            total: Some(8192),
            ..agreeing()
        };
        assert_eq!(place(&declined), Placement::Restart);
    }

    #[test]
    fn a_body_that_starts_where_the_file_ends_continues_it() {
        assert_eq!(place(&agreeing()), Placement::Continue);
    }

    #[test]
    fn a_body_that_starts_past_the_end_is_refused() {
        assert_eq!(
            place(&Landing {
                offset: 8192,
                ..agreeing()
            }),
            Placement::Refuse
        );
    }

    #[test]
    fn a_body_that_starts_before_the_end_is_refused() {
        assert_eq!(
            place(&Landing {
                offset: 1024,
                ..agreeing()
            }),
            Placement::Refuse
        );
    }

    #[test]
    fn an_empty_file_takes_a_body_from_the_start() {
        assert_eq!(
            place(&Landing {
                offset: 0,
                length_on_card: 0,
                expected: None,
                total: Some(8192),
            }),
            Placement::Restart
        );
    }

    #[test]
    fn a_tail_of_a_file_that_is_no_longer_the_one_promised_is_refused() {
        // The server's file now has a different length, so these bytes are
        // from a different file. Appending them could still give the expected
        // length, so only this check catches it.
        assert_eq!(
            place(&Landing {
                total: Some(9000),
                ..agreeing()
            }),
            Placement::Refuse
        );
    }

    #[test]
    fn a_server_that_gives_no_total_is_taken_at_its_word_about_the_offset() {
        // No total is not evidence of a change; refusing would make resuming
        // impossible with such a server.
        assert_eq!(
            place(&Landing {
                total: None,
                ..agreeing()
            }),
            Placement::Continue
        );
    }

    #[test]
    fn a_whole_file_answer_is_a_restart_even_when_the_length_changed() {
        // The body starts at zero, so it replaces whatever was there.
        assert_eq!(
            place(&Landing {
                offset: 0,
                total: Some(9000),
                ..agreeing()
            }),
            Placement::Restart
        );
    }

    fn sidecar(length: u32) -> Sidecar {
        Sidecar {
            length,
            etag: ETag::try_from("\"v1\"").ok(),
        }
    }

    #[test]
    fn nothing_on_the_card_means_fetch_the_whole_file() {
        assert_eq!(
            decide(&Cached {
                sidecar: None,
                length_on_card: None,
            }),
            Decision::Fetch
        );
    }

    #[test]
    fn a_file_as_long_as_its_sidecar_promises_is_ready_to_play() {
        assert_eq!(
            decide(&Cached {
                sidecar: Some(sidecar(4096)),
                length_on_card: Some(4096),
            }),
            Decision::Play
        );
    }

    #[test]
    fn a_short_file_resumes_from_where_it_stopped() {
        assert_eq!(
            decide(&Cached {
                sidecar: Some(sidecar(27841285)),
                length_on_card: Some(4096),
            }),
            Decision::Resume {
                from: 4096,
                etag: ETag::try_from("\"v1\"").ok(),
            }
        );
    }

    /// Only a sidecar confirms a file's length. Without one, a file that
    /// looks complete may be cut short.
    #[test]
    fn a_file_with_no_sidecar_is_incomplete_however_long_it_is() {
        assert_eq!(
            decide(&Cached {
                sidecar: None,
                length_on_card: Some(27841285),
            }),
            Decision::Fetch
        );
    }

    /// A sidecar with no file is what a crash between the two writes leaves
    /// behind.
    #[test]
    fn a_sidecar_with_no_file_beside_it_fetches_from_the_start() {
        assert_eq!(
            decide(&Cached {
                sidecar: Some(sidecar(4096)),
                length_on_card: None,
            }),
            Decision::Fetch
        );
    }

    /// Longer than expected cannot come from a correct download, so start
    /// again.
    #[test]
    fn a_file_longer_than_its_sidecar_promises_is_refetched_rather_than_trusted() {
        assert_eq!(
            decide(&Cached {
                sidecar: Some(sidecar(4096)),
                length_on_card: Some(8192),
            }),
            Decision::Fetch
        );
    }

    /// An empty file with a sidecar is normal right after the sidecar was
    /// written. Resuming from zero keeps the etag check.
    #[test]
    fn an_empty_file_resumes_from_zero_rather_than_starting_over() {
        assert_eq!(
            decide(&Cached {
                sidecar: Some(sidecar(4096)),
                length_on_card: Some(0),
            }),
            Decision::Resume {
                from: 0,
                etag: ETag::try_from("\"v1\"").ok(),
            }
        );
    }

    #[test]
    fn a_resume_carries_no_etag_when_the_sidecar_had_none() {
        let sidecar = Sidecar {
            length: 4096,
            etag: None,
        };
        assert_eq!(
            decide(&Cached {
                sidecar: Some(sidecar),
                length_on_card: Some(100),
            }),
            Decision::Resume {
                from: 100,
                etag: None,
            }
        );
    }
    /// The server's file still has the length in the sidecar, so the card's
    /// copy is kept.
    #[test]
    fn a_file_the_length_the_server_still_reports_is_fresh() {
        assert_eq!(revalidate(&sidecar(60975), Some(60975)), Freshness::Fresh);
    }

    /// A different length is the only sign of a change teddyCloud gives.
    #[test]
    fn a_different_length_means_the_cached_file_is_stale() {
        assert_eq!(revalidate(&sidecar(60975), Some(61000)), Freshness::Stale);
        assert_eq!(revalidate(&sidecar(60975), Some(1)), Freshness::Stale);
    }

    /// No length from the server is not a reason to throw the file away.
    #[test]
    fn a_server_that_gives_no_length_leaves_the_cached_file_alone() {
        assert_eq!(revalidate(&sidecar(60975), None), Freshness::Unknown);
    }

    /// A known weakness: a change that keeps the length is not detected.
    #[test]
    fn a_same_length_change_is_not_detectable() {
        let before = sidecar(60975);
        assert_eq!(revalidate(&before, Some(60975)), Freshness::Fresh);
    }
}
