//! What the box is doing, and what placing or lifting a figure changes.

use crate::{Action, Actions, PlaybackKind, Position, Prompt, TagUid};

/// Answers whether content is available locally, and where playback last
/// stopped. Implemented by the firmware over the SD card; stubbed in tests.
pub trait ContentIndex {
    fn is_available(&self, tag: TagUid) -> bool;
    fn saved_position(&self, tag: TagUid) -> Position;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
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
            State::Fetching(_) => PlaybackKind::Fetching,
            State::Playing(_) => PlaybackKind::Playing,
            State::Failed => PlaybackKind::Failed,
        }
    }

    pub fn current_tag(&self) -> Option<TagUid> {
        match self.state {
            State::Fetching(t) | State::Playing(t) => Some(t),
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
            State::Idle | State::Failed => {}
        }
        self.state = State::Idle;
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

    pub fn on_content_missing(&mut self, tag: TagUid, why: Unavailable) -> Actions {
        let mut actions = Actions::new();
        if self.state != State::Fetching(tag) {
            return actions;
        }
        self.state = State::Failed;
        let _ = actions.push(Action::PlayPrompt(match why {
            Unavailable::Unreachable => Prompt::NoNetwork,
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
    }

    impl ContentIndex for Index {
        fn is_available(&self, _tag: TagUid) -> bool {
            self.available
        }
        fn saved_position(&self, _tag: TagUid) -> Position {
            self.resume
        }
    }

    fn known(page: u32) -> Index {
        Index {
            available: true,
            resume: Position::Exact { page },
        }
    }

    fn unknown() -> Index {
        Index {
            available: false,
            resume: Position::default(),
        }
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
}
