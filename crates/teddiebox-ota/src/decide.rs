//! Whether to take an update, and why not when not.

use crate::Manifest;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The server's version is ours. Nothing to do.
    UpToDate,
    Update {
        length: u32,
    },
    Refuse(Refusal),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The image does not fit the slot it would be written into. Caught here
    /// rather than at the last sector, so no flash is touched at all.
    WillNotFit {
        length: u32,
        slot: u32,
    },
    /// Our own version is empty, which means the build did not stamp one.
    /// Refusing beats concluding that every manifest is an update and
    /// reflashing on every boot for ever.
    EmptyVersion,
    ZeroLength,
}

/// Compares for **difference**, never for order.
///
/// There is no ordering rule to get wrong, and a deliberate downgrade works —
/// which matters, because publishing the previous build is how a bad one gets
/// undone on a box nobody wants to open.
pub fn decide(manifest: &Manifest, ours: &str, slot_bytes: u32) -> Decision {
    if ours.is_empty() {
        return Decision::Refuse(Refusal::EmptyVersion);
    }
    if manifest.version.as_str() == ours {
        return Decision::UpToDate;
    }
    if manifest.length == 0 {
        return Decision::Refuse(Refusal::ZeroLength);
    }
    if manifest.length > slot_bytes {
        return Decision::Refuse(Refusal::WillNotFit {
            length: manifest.length,
            slot: slot_bytes,
        });
    }
    Decision::Update {
        length: manifest.length,
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::format;

    use super::*;
    use crate::{Manifest, MAX_MANIFEST};

    const DIGEST: &str = "3f786850e387550fdab836ed7e6dc881de23001b000000000000000000000000";
    const SLOT: u32 = 0x1D0000; // 1.86 MB, the 4 MB layout's slot

    fn manifest(version: &str, length: u32) -> Manifest {
        let s =
            format!("version = {version}\nsha256 = {DIGEST}\nlength = {length}\nimage = a.bin\n");
        Manifest::parse_read(s.as_bytes(), MAX_MANIFEST).unwrap()
    }

    #[test]
    fn the_same_version_is_up_to_date() {
        let m = manifest("2026-09-15-a1b2c3d", 1_103_728);
        assert_eq!(decide(&m, "2026-09-15-a1b2c3d", SLOT), Decision::UpToDate);
    }

    #[test]
    fn a_different_version_updates() {
        let m = manifest("2026-09-16-9f8e7d6", 1_103_728);
        assert_eq!(
            decide(&m, "2026-09-15-a1b2c3d", SLOT),
            Decision::Update { length: 1_103_728 }
        );
    }

    /// Difference, not order — so publishing yesterday's build is how a bad
    /// one gets undone.
    #[test]
    fn an_older_version_still_updates() {
        let m = manifest("2026-09-14-0000000", 1_103_728);
        assert_eq!(
            decide(&m, "2026-09-15-a1b2c3d", SLOT),
            Decision::Update { length: 1_103_728 }
        );
    }

    #[test]
    fn an_image_larger_than_the_slot_is_refused() {
        let m = manifest("v2", SLOT + 1);
        assert_eq!(
            decide(&m, "v1", SLOT),
            Decision::Refuse(Refusal::WillNotFit {
                length: SLOT + 1,
                slot: SLOT
            })
        );
    }

    #[test]
    fn an_image_exactly_the_size_of_the_slot_fits() {
        let m = manifest("v2", SLOT);
        assert_eq!(decide(&m, "v1", SLOT), Decision::Update { length: SLOT });
    }

    #[test]
    fn a_zero_length_image_is_refused() {
        let m = manifest("v2", 0);
        assert_eq!(
            decide(&m, "v1", SLOT),
            Decision::Refuse(Refusal::ZeroLength)
        );
    }

    /// Pins the guard order deliberately: the version check runs before the
    /// zero-length check. If our version already matches, there is nothing
    /// to do and no flash is touched — regardless of what a stale or
    /// malformed `length` says. Without this test, that order is an
    /// implicit contract nothing exercises: a reordering that made a
    /// same-version manifest with `length = 0` refuse instead of report
    /// up-to-date would pass every other test in this file.
    #[test]
    fn a_same_version_manifest_is_up_to_date_even_with_a_zero_length() {
        let m = manifest("v1", 0);
        assert_eq!(decide(&m, "v1", SLOT), Decision::UpToDate);
    }

    /// A box whose own version is empty has a build.rs that did not run. It
    /// must not conclude that every manifest is an update and reflash on
    /// every boot forever.
    #[test]
    fn an_empty_version_of_ours_refuses_rather_than_updating() {
        let m = manifest("v2", 1024);
        assert_eq!(
            decide(&m, "", SLOT),
            Decision::Refuse(Refusal::EmptyVersion)
        );
    }
}
