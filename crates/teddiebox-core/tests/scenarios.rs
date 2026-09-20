//! Whole-session walkthroughs. These read as a description of using the box.

use teddiebox_core::*;

const TAG: TagUid = TagUid([1, 2, 3, 4, 5, 6, 7, 8]);

struct Library {
    available: bool,
    resume: Position,
    /// Cached, and not yet asked about since the box booted.
    unchecked: bool,
}

impl ContentIndex for Library {
    fn is_available(&self, _tag: TagUid) -> bool {
        self.available
    }
    fn saved_position(&self, _tag: TagUid) -> Position {
        self.resume
    }
    fn wants_revalidation(&self, _tag: TagUid) -> bool {
        self.unchecked
    }
}

fn has(actions: &Actions, wanted: Action) -> bool {
    actions.contains(&wanted)
}

#[test]
fn a_child_plays_a_story_adjusts_the_volume_and_lifts_the_figure() {
    let mut core = Core::new(CoreConfig::default());
    let library = Library {
        available: true,
        resume: Position::Exact { page: 1 },
        unchecked: false,
    };

    let start = core.handle(Event::TagPresent(TAG), &library);
    assert!(has(
        &start,
        Action::Play {
            tag: TAG,
            from: Position::Exact { page: 1 }
        }
    ));

    core.handle(Event::EarDown(Ear::Larger, 1_000), &library);
    let louder = core.handle(Event::EarUp(Ear::Larger, 1_100), &library);
    assert!(louder.iter().any(|a| matches!(a, Action::SetVolume { .. })));

    let slap = core.handle(Event::Slap(Side::Right), &library);
    assert!(has(&slap, Action::NextTrack));

    let lift = core.handle(Event::TagAbsent, &library);
    assert!(lift
        .iter()
        .any(|a| matches!(a, Action::SavePosition { .. })));
    assert!(has(&lift, Action::Pause));
}

#[test]
fn an_unknown_figure_is_fetched_then_played() {
    let mut core = Core::new(CoreConfig::default());
    let empty = Library {
        available: false,
        resume: Position::default(),
        unchecked: false,
    };

    let placed = core.handle(Event::TagPresent(TAG), &empty);
    assert!(has(&placed, Action::RequestContent(TAG)));
    assert!(has(&placed, Action::SetLed(LedState::Fetching)));

    let downloaded = Library {
        available: true,
        resume: Position::Exact { page: 1 },
        unchecked: false,
    };
    let ready = core.handle(Event::ContentReady(TAG), &downloaded);
    assert!(has(
        &ready,
        Action::Play {
            tag: TAG,
            from: Position::Exact { page: 1 }
        }
    ));
}

#[test]
fn an_exhausted_pack_powers_the_box_off_mid_story() {
    let mut core = Core::new(CoreConfig::default());
    let library = Library {
        available: true,
        resume: Position::Exact { page: 1 },
        unchecked: false,
    };
    core.handle(Event::TagPresent(TAG), &library);

    // The model no longer believes a single sample: readings must agree
    // before the level, and the shutdown, commit (default readings_to_agree
    // is 4).
    for _ in 0..3 {
        core.handle(
            Event::Battery {
                pack_mv: 2_900,
                under_load: false,
            },
            &library,
        );
    }
    let flat = core.handle(
        Event::Battery {
            pack_mv: 2_900,
            under_load: false,
        },
        &library,
    );
    assert!(has(&flat, Action::PowerOff(PowerOffReason::PackEmpty)));
}

/// A figure placed for the first time this session, whose story is already on
/// the card. The box asks the server before it plays, hears that the file has
/// changed, and fetches the new one — the child hears the current story, not
/// the old one with a swap happening underneath it.
#[test]
fn a_cached_story_the_server_has_changed_is_fetched_before_it_plays() {
    let mut core = Core::new(CoreConfig::default());
    let cached = Library {
        available: true,
        resume: Position::Exact { page: 412 },
        unchecked: true,
    };

    let placed = core.handle(Event::TagPresent(TAG), &cached);
    assert!(has(&placed, Action::Revalidate(TAG)));
    assert!(
        !placed.iter().any(|a| matches!(a, Action::Play { .. })),
        "nothing plays until the server has answered: {placed:?}"
    );

    let stale = core.handle(Event::Revalidated(TAG, Freshness::Stale), &cached);
    assert!(has(&stale, Action::RequestContent(TAG)));

    // The refetch lands, and the story starts from the place the child left
    // it — a new file is not a reason to lose somebody's page.
    let ready = core.handle(Event::ContentReady(TAG), &cached);
    assert!(has(
        &ready,
        Action::Play {
            tag: TAG,
            from: Position::Exact { page: 412 }
        }
    ));
}

/// The offline case, which is the one that decides whether this is safe to
/// ship: the server could not be asked, so the cached story plays.
#[test]
fn a_story_the_server_could_not_be_asked_about_still_plays() {
    let mut core = Core::new(CoreConfig::default());
    let cached = Library {
        available: true,
        resume: Position::Start,
        unchecked: true,
    };

    core.handle(Event::TagPresent(TAG), &cached);
    let answered = core.handle(Event::Revalidated(TAG, Freshness::Current), &cached);

    assert!(has(
        &answered,
        Action::Play {
            tag: TAG,
            from: Position::Start
        }
    ));
}
