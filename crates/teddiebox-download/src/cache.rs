//! What to do about a file that may or may not already be on the card.
//!
//! Every branch here is one the box will hit: a download interrupted by a
//! lifted Tonie, by a flat battery, or between the two writes that create a
//! cache entry. The rules are in one pure function so that all of them can be
//! stated as tests rather than discovered on a card.

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
    // No sidecar means nothing vouches for the file's length, and an
    // unvouched file may be a story that stops in the middle.
    let (Some(sidecar), Some(on_card)) = (&cached.sidecar, cached.length_on_card) else {
        return Decision::Fetch;
    };

    match on_card.cmp(&sidecar.length) {
        core::cmp::Ordering::Equal => Decision::Play,
        core::cmp::Ordering::Less => Decision::Resume {
            from: on_card,
            etag: sidecar.etag.clone(),
        },
        // Longer than promised cannot come from a correct download. Something
        // is wrong with one of the two files, and refetching is both cheap and
        // certain where trusting it is neither.
        core::cmp::Ordering::Greater => Decision::Fetch,
    }
}

/// Whether what reached the card adds up to the whole file.
///
/// `from` is where this body was written, `written` how much of it landed, and
/// `total` what the server said the whole file is.
///
/// This exists because "the transfer stopped" and "the story is ready" are not
/// the same statement, and the firmware used to make the second one whenever
/// the first was true — announcing a fragment as a finished story. A download
/// can stop early for reasons that leave a perfectly valid partial file
/// behind: a figure lifted, a socket dropped, a server that closed the
/// connection. Only the arithmetic says which happened.
///
/// A file whose length the server never gave can never be called whole. That
/// is the same bargain the sidecar makes: nothing is claimed that was not
/// said.
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
/// Three things can go wrong, and only the first is obvious.
///
/// A server may **decline the range**, answering `200` with the whole file;
/// that arrives as `offset: 0`, and appending it to a partial download would
/// splice the beginning of the story onto its middle. Restarting is the only
/// correct answer, and the writer has to take it, because by then the request
/// is long sent.
///
/// A server may **honour the range against a file that has changed**. Nothing
/// in the response says so — the offset is exactly what was asked for — and the
/// result would be the tail of one file appended to the head of another,
/// finishing at precisely the length the sidecar promised. That is the one
/// corruption a length check can never catch, so the lengths are compared here
/// instead: a total that disagrees with what the sidecar recorded means the
/// bytes in hand belong to a file whose beginning is not on the card. There is
/// nothing to salvage, because a body starting mid-file cannot be written from
/// the start; refusing costs a refetch, which is the cheap half of the trade.
///
/// A server may simply **place bytes somewhere the file cannot take them**,
/// leaving a gap or an overlap. Same answer, same reason.
///
/// Silence is not evidence: a server that sends no total has said nothing about
/// whether its copy changed, and refusing on that would make resume impossible
/// against a server that never sends a length.
pub fn place(landing: &Landing) -> Placement {
    // Nothing is spliced when the body is the whole file, so this holds
    // whether or not the server's copy is still the one that was promised.
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
/// **This is a weaker check than it looks, and deliberately so.** The design
/// this project started from revalidated with `If-None-Match` and a `304`, but
/// the teddyCloud on this LAN sends **no `ETag`** — nor `Last-Modified`, nor
/// `Cache-Control` — on any content route. Measured, not assumed. The only
/// thing it offers to compare is `Content-Length`, so that is what this
/// compares, and a change that keeps the byte count is invisible to it. For a
/// figure's audio, which does not change silently, that is a reasonable trade;
/// stating it here is better than an ETag path that never runs.
///
/// **Silence keeps the file.** A server that gives no length has said nothing
/// about whether ours is stale, and throwing away a good file on no evidence
/// costs a child their story for as long as the download takes.
///
/// Nothing here fetches. The caller decides whether a probe is worth a round
/// trip at all — and the design's rule that revalidation must never interrupt
/// playback means the answer, on the path to a story, is usually no.
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

    /// A download that stops early still leaves a file on the card, and on
    /// 2026-09-07 the box announced one such file — 1,089,536 bytes of a
    /// 38,349,983-byte story — as a finished story and tried to play it. The
    /// decoder rejected it, which was luck: a fragment that happened to parse
    /// would have played as a story that stops in the middle.
    #[test]
    fn a_body_that_reaches_the_servers_length_is_whole() {
        assert!(is_whole(0, 38_349_983, Some(38_349_983)));
    }

    #[test]
    fn a_body_that_stops_early_is_not_whole() {
        assert!(!is_whole(0, 1_089_536, Some(38_349_983)));
    }

    /// The common case after an interruption: the file is finished by a body
    /// that never contained its beginning.
    #[test]
    fn a_resumed_body_that_finishes_the_file_is_whole() {
        assert!(is_whole(3_145_728, 35_204_255, Some(38_349_983)));
    }

    /// Nothing said how long the file was, so nothing can claim it is all
    /// there — the same bargain `write_sidecar` makes when it declines to
    /// vouch for a length the server never gave.
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

    /// A resume that everything agrees about: half a 8192-byte file is there,
    /// and the server is sending the rest of that same file.
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
        // The server honoured the range against a file of a different length,
        // so these bytes are the middle of something whose beginning is not on
        // the card. Appending them would leave the file exactly as long as the
        // sidecar promised, which is the one thing a length check cannot catch.
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
        // Silence is not evidence of a changed file, and refusing on it would
        // make resume impossible against a server that never sends a length.
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
        // Nothing is being spliced: the body starts at zero, so it replaces
        // whatever was there whether or not it is the same file.
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

    /// The rule the whole design rests on. A sidecar is the only thing that
    /// vouches for a file's length; without one, a file that looks complete
    /// may be a story that stops in the middle, and the box would never know.
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

    /// A sidecar with no file is what a crash between the two writes leaves.
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

    /// Longer than promised cannot happen from a correct download, so
    /// something is wrong with one of the two files. Refetching is cheap and
    /// certain; trusting it risks handing the decoder a spliced file.
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

    /// An empty file plus a sidecar is the ordinary state right after the
    /// sidecar was written. Resuming from zero keeps the etag check that a
    /// plain fetch would drop.
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
    /// The ordinary case: the server still has a file of the length the
    /// sidecar recorded, so what is on the card stands.
    #[test]
    fn a_file_the_length_the_server_still_reports_is_fresh() {
        assert_eq!(revalidate(&sidecar(60975), Some(60975)), Freshness::Fresh);
    }

    /// A different length is the only evidence of change this server offers.
    #[test]
    fn a_different_length_means_the_cached_file_is_stale() {
        assert_eq!(revalidate(&sidecar(60975), Some(61000)), Freshness::Stale);
        assert_eq!(revalidate(&sidecar(60975), Some(1)), Freshness::Stale);
    }

    /// A server that will not say how long the file is has said nothing about
    /// whether ours is current — which is not the same as saying it is wrong.
    /// Discarding a good file on no evidence costs a child their story for the
    /// length of a download, so silence keeps what is already there.
    #[test]
    fn a_server_that_gives_no_length_leaves_the_cached_file_alone() {
        assert_eq!(revalidate(&sidecar(60975), None), Freshness::Unknown);
    }

    /// Length is a weak validator and this pins the weakness rather than
    /// hiding it: an edit that keeps the byte count is invisible here. It is
    /// what this server makes available, and the alternative was pretending an
    /// ETag exists.
    #[test]
    fn a_same_length_change_is_not_detectable() {
        let before = sidecar(60975);
        assert_eq!(revalidate(&before, Some(60975)), Freshness::Fresh);
    }
}
