//! What the box is doing, and what placing or lifting a figure changes.

use crate::{Action, Actions, PlaybackKind, Position, Prompt, TagUid};

/// Answers whether content is available locally, and where playback last
/// stopped. Implemented by the firmware over the SD card; stubbed in tests.
pub trait ContentIndex {
    fn is_available(&self, tag: TagUid) -> bool;
    fn saved_position(&self, tag: TagUid) -> Position;
    /// Whether this figure's story should be checked against the server
    /// before it plays.
    ///
    /// A narrower question than [`Self::is_available`], and deliberately not
    /// folded into it: one asks whether there is anything to play, the other
    /// whether what there is has been questioned lately. Only cached content
    /// can answer yes — a file shipped under `CONTENT/` has no sidecar to
    /// compare a length against, and was never downloaded.
    fn wants_revalidation(&self, tag: TagUid) -> bool;
}

/// What the server said about a story already on the card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Nothing contradicted the cached copy — including a server that could
    /// not be asked at all. Offline is not a reason to refuse a story.
    Current,
    /// The server has a different file under this figure's name.
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    /// Waiting to hear whether the cached story is still the server's.
    Checking(TagUid),
    Fetching(TagUid),
    Playing(TagUid),
    Failed,
}

/// Why a figure's story could not be produced.
///
/// The split is what a person holding the box can act on. A network they can
/// go and look at is worth naming; a figure the server simply has no story
/// for is not their fault and not their problem to fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    /// Association, DHCP, TLS, the socket, a timeout, or a server that
    /// answered with a fault of its own.
    Unreachable,
    /// The access point was there and turned the box away: the passphrase on
    /// the card is not the one it wants.
    ///
    /// Deliberately narrow. Everything a caller is not *sure* about belongs in
    /// [`Unavailable::Unreachable`], because the expensive mistake is the other
    /// one — telling somebody their passphrase is wrong when the router was
    /// merely off sends them to retype something that was already correct.
    Refused,
    /// The server was reached and has nothing for this figure.
    NoContent,
}

#[derive(Debug)]
pub struct Playback {
    state: State,
    position: Position,
}

impl Default for Playback {
    fn default() -> Self {
        Self {
            state: State::Idle,
            position: Position::default(),
        }
    }
}

impl Playback {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn kind(&self) -> PlaybackKind {
        match self.state {
            State::Idle => PlaybackKind::Idle,
            // Told apart internally, and not outside: to everything watching,
            // a box waiting on the network is a box waiting on the network.
            State::Checking(_) | State::Fetching(_) => PlaybackKind::Fetching,
            State::Playing(_) => PlaybackKind::Playing,
            State::Failed => PlaybackKind::Failed,
        }
    }

    pub fn current_tag(&self) -> Option<TagUid> {
        match self.state {
            State::Checking(t) | State::Fetching(t) | State::Playing(t) => Some(t),
            _ => None,
        }
    }

    /// Records how far playback has advanced, so lifting the figure can save it.
    pub fn note_position(&mut self, pos: Position) {
        self.position = pos;
    }

    pub fn on_tag_present<I: ContentIndex>(&mut self, tag: TagUid, index: &I) -> Actions {
        let mut actions = Actions::new();
        if index.is_available(tag) {
            // Asked before it plays, not behind it. A stale story swapped
            // underneath a listening child would need two handles on one file,
            // which `embedded-sdmmc` refuses, and a rename, which it does not
            // have.
            if index.wants_revalidation(tag) {
                self.state = State::Checking(tag);
                let _ = actions.push(Action::Revalidate(tag));
                return actions;
            }
            let from = index.saved_position(tag);
            self.position = from;
            self.state = State::Playing(tag);
            let _ = actions.push(Action::Play { tag, from });
        } else {
            self.state = State::Fetching(tag);
            let _ = actions.push(Action::RequestContent(tag));
        }
        actions
    }

    pub fn on_tag_absent(&mut self) -> Actions {
        let mut actions = Actions::new();
        match self.state {
            State::Playing(tag) => {
                let _ = actions.push(Action::SavePosition {
                    tag,
                    pos: self.position,
                });
                let _ = actions.push(Action::Pause);
            }
            // Nobody is waiting for these bytes any more. What is already on
            // the card keeps its sidecar, so placing the figure again resumes
            // instead of starting over.
            State::Fetching(_) => {
                let _ = actions.push(Action::AbortFetch);
            }
            // Nothing was fetched, so there is nothing to abort. The answer
            // still arriving is discarded by the identity guard in
            // `on_revalidated`.
            State::Checking(_) | State::Idle | State::Failed => {}
        }
        self.state = State::Idle;
        actions
    }

    /// What the server said about a cached story, and what to do about it.
    pub fn on_revalidated<I: ContentIndex>(
        &mut self,
        tag: TagUid,
        freshness: Freshness,
        index: &I,
    ) -> Actions {
        let mut actions = Actions::new();
        // The same guard the download outcome has: an answer that arrives
        // after the figure was lifted or swapped belongs to nobody.
        if self.state != State::Checking(tag) {
            return actions;
        }
        match freshness {
            Freshness::Current => {
                let from = index.saved_position(tag);
                self.position = from;
                self.state = State::Playing(tag);
                let _ = actions.push(Action::Play { tag, from });
            }
            // The path a figure with no content at all takes. One refetch, not
            // two, so the resume rules and the failure words stay in one place.
            Freshness::Stale => {
                self.state = State::Fetching(tag);
                let _ = actions.push(Action::RequestContent(tag));
            }
        }
        actions
    }

    pub fn on_content_ready<I: ContentIndex>(&mut self, tag: TagUid, index: &I) -> Actions {
        let mut actions = Actions::new();
        // Ignore a download that finished after the figure was lifted or swapped.
        if self.state != State::Fetching(tag) {
            return actions;
        }
        let from = index.saved_position(tag);
        self.position = from;
        self.state = State::Playing(tag);
        let _ = actions.push(Action::Play { tag, from });
        actions
    }

    /// The story reached its end on its own.
    ///
    /// Distinct from a lift: nothing is saved, because a finished story is not
    /// a paused one and its place has just been cleared. Distinct from
    /// `TrackFinished`, which advances a chapter — a story ending and a chapter
    /// ending are different facts and sharing an event for them is how the
    /// second one would silently acquire the first one's consequences.
    ///
    /// Only a story that was playing can end. A fetch in progress is left
    /// alone, because the sound that just finished was something else.
    pub fn on_playback_ended(&mut self) -> Actions {
        if matches!(self.state, State::Playing(_)) {
            self.state = State::Idle;
        }
        Actions::new()
    }

    pub fn on_content_missing(&mut self, tag: TagUid, why: Unavailable) -> Actions {
        let mut actions = Actions::new();
        if self.state != State::Fetching(tag) {
            return actions;
        }
        self.state = State::Failed;
        let _ = actions.push(Action::PlayPrompt(match why {
            Unavailable::Unreachable => Prompt::NoNetwork,
            Unavailable::Refused => Prompt::WrongPassword,
            Unavailable::NoContent => Prompt::NoContent,
        }));
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use heapless::Vec;

    const TAG: TagUid = TagUid([1, 2, 3, 4, 5, 6, 7, 8]);
    const OTHER: TagUid = TagUid([9, 9, 9, 9, 9, 9, 9, 9]);

    struct Index {
        available: bool,
        resume: Position,
        wants_asking: bool,
    }

    impl ContentIndex for Index {
        fn is_available(&self, _tag: TagUid) -> bool {
            self.available
        }
        fn saved_position(&self, _tag: TagUid) -> Position {
            self.resume
        }
        fn wants_revalidation(&self, _tag: TagUid) -> bool {
            self.wants_asking
        }
    }

    fn known(page: u32) -> Index {
        Index {
            available: true,
            resume: Position::Exact { page },
            wants_asking: false,
        }
    }

    fn unknown() -> Index {
        Index {
            available: false,
            resume: Position::default(),
            wants_asking: false,
        }
    }

    /// Cached, complete, and not yet asked about since the box booted.
    fn unchecked(page: u32) -> Index {
        Index {
            available: true,
            resume: Position::Exact { page },
            wants_asking: true,
        }
    }

    /// Decision: a story whose cached copy may be out of
    /// date is not played while a fresh one arrives behind it. The box asks
    /// first, and the answer decides what happens.
    #[test]
    fn a_figure_that_has_not_been_asked_about_is_asked_before_it_plays() {
        let mut p = Playback::new();
        let actions = p.on_tag_present(TAG, &unchecked(1));
        assert_eq!(actions.as_slice(), &[Action::Revalidate(TAG)]);
        assert_eq!(p.kind(), PlaybackKind::Fetching);
    }

    #[test]
    fn a_story_the_server_agrees_with_plays_from_where_it_stopped() {
        let mut p = Playback::new();
        p.on_tag_present(TAG, &unchecked(412));
        let actions = p.on_revalidated(TAG, Freshness::Current, &unchecked(412));
        assert_eq!(
            actions.as_slice(),
            &[Action::Play {
                tag: TAG,
                from: Position::Exact { page: 412 }
            }]
        );
        assert_eq!(p.kind(), PlaybackKind::Playing);
    }

    /// A stale story takes the same path a figure with no story at all takes.
    /// One refetch code path, not two — and the child hears the new version,
    /// not the old one with a swap happening underneath it.
    #[test]
    fn a_stale_story_is_fetched_again_rather_than_played() {
        let mut p = Playback::new();
        p.on_tag_present(TAG, &unchecked(412));
        let actions = p.on_revalidated(TAG, Freshness::Stale, &unchecked(412));
        assert_eq!(actions.as_slice(), &[Action::RequestContent(TAG)]);
        assert_eq!(p.kind(), PlaybackKind::Fetching);
    }

    /// The same identity guard the download outcome has. A probe that finishes
    /// after the figure was lifted or swapped belongs to nobody.
    #[test]
    fn an_answer_for_a_figure_no_longer_on_the_plate_is_ignored() {
        let mut p = Playback::new();
        p.on_tag_present(TAG, &unchecked(1));
        let actions = p.on_revalidated(OTHER, Freshness::Stale, &unchecked(1));
        assert!(actions.as_slice().is_empty());
    }

    /// Lifting a figure mid-question stops the box waiting for an answer it no
    /// longer has a use for. Nothing is aborted, because nothing was fetched.
    #[test]
    fn lifting_a_figure_that_is_being_asked_about_aborts_nothing() {
        let mut p = Playback::new();
        p.on_tag_present(TAG, &unchecked(1));
        let actions = p.on_tag_absent();
        assert!(actions.as_slice().is_empty());
        assert_eq!(p.kind(), PlaybackKind::Idle);
    }

    /// A figure asked about earlier in the session plays without a word to the
    /// server: the radio is the largest consumer on this pack.
    #[test]
    fn a_figure_already_asked_about_plays_straight_away() {
        let mut p = Playback::new();
        let actions = p.on_tag_present(TAG, &known(3));
        assert_eq!(
            actions.as_slice(),
            &[Action::Play {
                tag: TAG,
                from: Position::Exact { page: 3 }
            }]
        );
    }

    #[test]
    fn placing_a_known_figure_plays_from_the_start() {
        let mut p = Playback::new();
        let actions = p.on_tag_present(TAG, &known(1));
        assert_eq!(
            actions.as_slice(),
            &[Action::Play {
                tag: TAG,
                from: Position::Exact { page: 1 }
            }]
        );
        assert_eq!(p.kind(), PlaybackKind::Playing);
    }

    #[test]
    fn placing_a_known_figure_resumes_where_it_stopped() {
        let mut p = Playback::new();
        let actions = p.on_tag_present(TAG, &known(412));
        assert_eq!(
            actions.as_slice(),
            &[Action::Play {
                tag: TAG,
                from: Position::Exact { page: 412 }
            }]
        );
    }

    #[test]
    fn placing_an_unknown_figure_requests_the_content() {
        let mut p = Playback::new();
        let actions = p.on_tag_present(TAG, &unknown());
        assert_eq!(actions.as_slice(), &[Action::RequestContent(TAG)]);
        assert_eq!(p.kind(), PlaybackKind::Fetching);
    }

    #[test]
    fn lifting_the_figure_saves_the_position_and_pauses() {
        let mut p = Playback::new();
        p.on_tag_present(TAG, &known(1));
        p.note_position(Position::Exact { page: 77 });

        let actions: Vec<Action, 8> = p.on_tag_absent();
        assert_eq!(
            actions.as_slice(),
            &[
                Action::SavePosition {
                    tag: TAG,
                    pos: Position::Exact { page: 77 }
                },
                Action::Pause,
            ]
        );
        assert_eq!(p.kind(), PlaybackKind::Idle);
    }

    #[test]
    fn lifting_a_figure_that_was_never_placed_does_nothing() {
        let mut p = Playback::new();
        assert!(p.on_tag_absent().is_empty());
    }

    #[test]
    fn replacing_the_same_figure_resumes_from_the_saved_position() {
        let mut p = Playback::new();
        p.on_tag_present(TAG, &known(1));
        p.note_position(Position::Exact { page: 77 });
        p.on_tag_absent();

        let actions = p.on_tag_present(TAG, &known(77));
        assert_eq!(
            actions.as_slice(),
            &[Action::Play {
                tag: TAG,
                from: Position::Exact { page: 77 }
            }]
        );
    }

    #[test]
    fn a_completed_download_starts_playback() {
        let mut p = Playback::new();
        p.on_tag_present(TAG, &unknown());
        let actions = p.on_content_ready(TAG, &known(1));
        assert_eq!(
            actions.as_slice(),
            &[Action::Play {
                tag: TAG,
                from: Position::Exact { page: 1 }
            }]
        );
        assert_eq!(p.kind(), PlaybackKind::Playing);
    }

    #[test]
    fn a_download_completing_for_a_figure_no_longer_present_is_ignored() {
        let mut p = Playback::new();
        p.on_tag_present(TAG, &unknown());
        assert!(p.on_content_ready(OTHER, &known(1)).is_empty());
        assert_eq!(p.kind(), PlaybackKind::Fetching);
    }

    #[test]
    fn a_figure_the_server_has_no_story_for_says_so() {
        let mut p = Playback::new();
        p.on_tag_present(TAG, &unknown());
        let actions = p.on_content_missing(TAG, Unavailable::NoContent);
        assert_eq!(actions.as_slice(), &[Action::PlayPrompt(Prompt::NoContent)]);
        assert_eq!(p.kind(), PlaybackKind::Failed);
    }

    /// The split a person can act on: a network they can go and look at,
    /// against a figure nothing can be done about.
    #[test]
    fn a_figure_that_could_not_be_reached_blames_the_network() {
        let mut p = Playback::new();
        p.on_tag_present(TAG, &unknown());
        let actions = p.on_content_missing(TAG, Unavailable::Unreachable);
        assert_eq!(actions.as_slice(), &[Action::PlayPrompt(Prompt::NoNetwork)]);
        assert_eq!(p.kind(), PlaybackKind::Failed);
    }

    /// A network that refused the box is not a network that was not there, and
    /// the two send whoever is holding the box to different places: one to the
    /// passphrase on the card, the other to the router. Saying "no internet"
    /// for a refused passphrase sends them to look at a router that is working.
    #[test]
    fn a_figure_whose_network_refused_the_passphrase_blames_the_passphrase() {
        let mut p = Playback::new();
        p.on_tag_present(TAG, &unknown());
        let actions = p.on_content_missing(TAG, Unavailable::Refused);
        assert_eq!(
            actions.as_slice(),
            &[Action::PlayPrompt(Prompt::WrongPassword)]
        );
        assert_eq!(p.kind(), PlaybackKind::Failed);
    }

    #[test]
    fn a_failure_for_a_figure_already_lifted_is_ignored() {
        let mut p = Playback::new();
        p.on_tag_present(TAG, &unknown());
        let actions = p.on_content_missing(OTHER, Unavailable::NoContent);
        assert!(actions.is_empty());
    }

    /// Lifting a figure mid-download stops the download. The partial file and
    /// its sidecar stay on the card, so placing it again resumes rather than
    /// starting over — which is only cheap because resume works.
    #[test]
    fn lifting_a_figure_that_is_still_fetching_abandons_the_download() {
        let mut p = Playback::new();
        p.on_tag_present(TAG, &unknown());
        assert_eq!(p.kind(), PlaybackKind::Fetching);
        let actions = p.on_tag_absent();
        assert_eq!(actions.as_slice(), &[Action::AbortFetch]);
        assert_eq!(p.kind(), PlaybackKind::Idle);
    }

    #[test]
    fn lifting_a_playing_figure_does_not_abort_anything() {
        let mut p = Playback::new();
        p.on_tag_present(TAG, &known(7));
        let actions = p.on_tag_absent();
        assert_eq!(
            actions.as_slice(),
            &[
                Action::SavePosition {
                    tag: TAG,
                    pos: Position::Exact { page: 7 }
                },
                Action::Pause
            ]
        );
    }

    #[test]
    fn lifting_from_an_empty_plate_does_nothing() {
        let mut p = Playback::new();
        assert!(p.on_tag_absent().is_empty());
    }

    /// A story that reaches its end leaves the box idle, with the figure still on
    /// the plate. Until this existed the only way out of `Playing` was lifting the
    /// figure, so a finished story pinned the indicator and made the idle timeout
    /// — gated on not-playing — unable to fire at all.
    #[test]
    fn a_story_reaching_its_end_leaves_the_box_idle() {
        let mut p = Playback::new();
        p.on_tag_present(TAG, &known(1));
        let actions = p.on_playback_ended();
        assert!(actions.is_empty());
        assert_eq!(p.kind(), PlaybackKind::Idle);
    }

    /// Not `SavePosition`: a finished story is not a paused one, and its place has
    /// just been cleared on purpose. Not `Pause` either — nothing is playing.
    #[test]
    fn a_story_reaching_its_end_saves_no_position() {
        let mut p = Playback::new();
        p.on_tag_present(TAG, &known(1));
        p.note_position(Position::Exact { page: 900 });
        assert!(p.on_playback_ended().is_empty());
    }

    /// A story ending while the box was fetching something else, or idle already,
    /// changes nothing.
    #[test]
    fn an_end_with_nothing_playing_is_ignored() {
        let mut p = Playback::new();
        p.on_tag_present(TAG, &unknown());
        assert!(p.on_playback_ended().is_empty());
        assert_eq!(
            p.kind(),
            PlaybackKind::Fetching,
            "a fetch is not interrupted"
        );
    }
}
