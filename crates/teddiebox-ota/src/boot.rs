//! Whether this boot should confirm itself, revert, or do nothing.
//!
//! This box's bootloader ignores the OTA state: a slot set to `New` stays
//! `New` across reboots. So the app runs the `New -> PendingVerify -> Valid`
//! state machine itself, using the state field in `otadata`. Since the
//! bootloader never writes that field, the app can safely own it.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    /// A slot just selected, by an update or by the `otaboot` console
    /// command. Not booted since.
    New,
    /// A previous boot of this slot marked it `PendingVerify` and did not
    /// reach `Valid` before rebooting: the new image failed.
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
    /// Still `PendingVerify`, so the last attempt never reached `Valid`.
    /// Switch back to the other slot now, before this attempt fails the same
    /// way.
    Revert,
    /// Nothing to confirm.
    Proceed,
}

/// A slot gets one `New` boot to become `PendingVerify`, and one
/// `PendingVerify` boot to become `Valid`. Finding `PendingVerify` again
/// means that boot failed, so revert.
pub fn boot_action(state: SlotState) -> BootAction {
    match state {
        SlotState::New => BootAction::ConfirmFirstBoot,
        SlotState::PendingVerify => BootAction::Revert,
        SlotState::Confirmed => BootAction::Proceed,
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
    fn a_slot_still_pending_from_a_previous_boot_reverts() {
        // Given
        let state = SlotState::PendingVerify;

        // When
        let action = boot_action(state);

        // Then
        assert_eq!(action, BootAction::Revert);
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
