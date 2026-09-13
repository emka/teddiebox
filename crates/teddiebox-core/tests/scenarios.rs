//! Whole-session walkthroughs. These read as a description of using the box.

use teddiebox_core::*;

const TAG: TagUid = TagUid([1, 2, 3, 4, 5, 6, 7, 8]);

struct Library {
    available: bool,
    resume: Position,
}

impl ContentIndex for Library {
    fn is_available(&self, _tag: TagUid) -> bool {
        self.available
    }
    fn saved_position(&self, _tag: TagUid) -> Position {
        self.resume
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
    assert!(louder.iter().any(|a| matches!(a, Action::SetVolume(_))));

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
    };

    let placed = core.handle(Event::TagPresent(TAG), &empty);
    assert!(has(&placed, Action::RequestContent(TAG)));
    assert!(has(&placed, Action::SetLed(LedState::Fetching)));

    let downloaded = Library {
        available: true,
        resume: Position::Exact { page: 1 },
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
