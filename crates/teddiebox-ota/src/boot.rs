//! Whether this boot should confirm itself, revert, or do nothing.
//!
//! This bootloader does not act on OTA state at all — a slot armed `New`
//! stayed `New` across three measured reboots, never promoted to
//! `PendingVerify`, with nothing validating it (spec §7a). So the app must
//! run this state machine itself, using the same `New -> PendingVerify ->
//! Valid` field `otadata` already has room for. The bootloader ignoring the
//! field is exactly what makes it safe for the app to own it: nothing else
//! ever writes it.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    /// A slot just selected, either by a real update or by the bench's
    /// `otaboot` command. Never booted since.
    New,
    /// A previous boot of this slot marked it `PendingVerify` and did not
    /// reach `Valid` before rebooting — the attempt this state exists to
    /// catch.
    PendingVerify,
    /// Everything else: `Valid` (the ordinary running case), `Invalid`,
    /// `Aborted`, or `Undefined` (a box that has never run an update).
    /// Collapsed into one variant because the app does the same thing for
    /// all of them: nothing.
    Confirmed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootAction {
    /// First boot of a freshly-selected slot. Mark it `PendingVerify` before
    /// anything else runs, so a crash on this attempt is visible on the next
    /// boot rather than silent.
    ConfirmFirstBoot,
    /// `PendingVerify` survived to this boot, so the last attempt never
    /// reached `Valid`. Switch to the other slot now, before this attempt
    /// gets a chance to fail the same way again.
    Revert,
    /// Nothing to confirm.
    Proceed,
}

/// One retry per flash: a slot gets exactly one `New` boot to become
/// `PendingVerify`, and exactly one `PendingVerify` boot to become `Valid`.
/// Finding `PendingVerify` a second time is the only signal this state
/// machine has, and it means the second thing, not a third chance.
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
        assert_eq!(boot_action(SlotState::New), BootAction::ConfirmFirstBoot);
    }

    #[test]
    fn a_slot_still_pending_from_a_previous_boot_reverts() {
        assert_eq!(boot_action(SlotState::PendingVerify), BootAction::Revert);
    }

    #[test]
    fn a_confirmed_slot_does_nothing() {
        assert_eq!(boot_action(SlotState::Confirmed), BootAction::Proceed);
    }
}
