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
    use teddiebox_cloud::ETag;

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
