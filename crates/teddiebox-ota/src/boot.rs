//! Whether this boot should confirm itself, revert, or do nothing.
//!
//! The bootloader owns the rollback. On the first boot of a slot set to `New`
//! it marks the slot `PendingVerify`, and if the app resets before marking it
//! `Valid` it marks the slot `Aborted` and boots the other one. The app only
//! has to reach `Valid`; it must not revert on seeing `PendingVerify`, which is
//! what every first boot of a new slot looks like.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    /// A slot just selected, by an update or by the `otaboot` console
    /// command. Not booted since.
    New,
    /// The bootloader booted this slot for the first time and is waiting for
    /// the app to mark it `Valid`.
    PendingVerify,
    /// Everything else: `Valid` (the normal case), `Invalid`, `Aborted`, or
    /// `Undefined` (a box that has never updated). One variant, because the
    /// app does nothing in all of them.
    Confirmed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootAction {
    /// First boot of a newly selected slot. Mark it `PendingVerify` before
    /// anything else runs, so a crash is noticed on the next boot.
    ConfirmFirstBoot,
    /// Switch back to the other slot now. Nothing asks for this while the
    /// bootloader does the rolling back.
    Revert,
    /// Nothing to confirm.
    Proceed,
}

/// A `New` slot is one the bootloader has not marked yet, so the app marks it.
/// `PendingVerify` is the bootloader's own mark, so the app carries on and
/// reaches `Valid` through `mark_valid`.
pub fn boot_action(state: SlotState) -> BootAction {
    match state {
        SlotState::New => BootAction::ConfirmFirstBoot,
        SlotState::PendingVerify | SlotState::Confirmed => BootAction::Proceed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_freshly_selected_slot_confirms_its_first_boot() {
        // Given
        let state = SlotState::New;

        // When
        let action = boot_action(state);

        // Then
        assert_eq!(action, BootAction::ConfirmFirstBoot);
    }

    #[test]
    fn a_slot_the_bootloader_left_pending_proceeds() {
        // Given
        let state = SlotState::PendingVerify;

        // When
        let action = boot_action(state);

        // Then
        assert_eq!(action, BootAction::Proceed);
    }

    #[test]
    fn a_confirmed_slot_does_nothing() {
        // Given
        let state = SlotState::Confirmed;

        // When
        let action = boot_action(state);

        // Then
        assert_eq!(action, BootAction::Proceed);
    }
}
