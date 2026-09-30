//! Whether to install an update, and if not, why.

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
    /// The image does not fit its slot. Caught before any flash is written.
    WillNotFit {
        length: u32,
        slot: u32,
    },
    /// Our own version is empty, so the build did not set one. Otherwise
    /// every manifest would look like an update, and the box would reflash on
    /// every boot.
    EmptyVersion,
    ZeroLength,
}

/// Compares for **difference**, never for order.
///
/// There is no version ordering to get wrong, and a downgrade works: to undo
/// a bad build, publish the previous one.
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
        // Given
        let m = manifest("2026-09-15-a1b2c3d", 1_103_728);

        // When
        let decision = decide(&m, "2026-09-15-a1b2c3d", SLOT);

        // Then
        assert_eq!(decision, Decision::UpToDate);
    }

    #[test]
    fn a_different_version_updates() {
        // Given
        let m = manifest("2026-09-16-9f8e7d6", 1_103_728);

        // When
        let decision = decide(&m, "2026-09-15-a1b2c3d", SLOT);

        // Then
        assert_eq!(decision, Decision::Update { length: 1_103_728 });
    }

    /// Any different version is installed, so a bad build can be undone by
    /// publishing the previous one.
    #[test]
    fn an_older_version_still_updates() {
        // Given
        let m = manifest("2026-09-14-0000000", 1_103_728);

        // When
        let decision = decide(&m, "2026-09-15-a1b2c3d", SLOT);

        // Then
        assert_eq!(decision, Decision::Update { length: 1_103_728 });
    }

    #[test]
    fn an_image_larger_than_the_slot_is_refused() {
        // Given
        let m = manifest("v2", SLOT + 1);

        // When
        let decision = decide(&m, "v1", SLOT);

        // Then
        assert_eq!(
            decision,
            Decision::Refuse(Refusal::WillNotFit {
                length: SLOT + 1,
                slot: SLOT
            })
        );
    }

    #[test]
    fn an_image_exactly_the_size_of_the_slot_fits() {
        // Given
        let m = manifest("v2", SLOT);

        // When
        let decision = decide(&m, "v1", SLOT);

        // Then
        assert_eq!(decision, Decision::Update { length: SLOT });
    }

    #[test]
    fn a_zero_length_image_is_refused() {
        // Given
        let m = manifest("v2", 0);

        // When
        let decision = decide(&m, "v1", SLOT);

        // Then
        assert_eq!(decision, Decision::Refuse(Refusal::ZeroLength));
    }

    /// The version check comes before the length check: if the version
    /// matches, nothing happens, whatever `length` says.
    #[test]
    fn a_same_version_manifest_is_up_to_date_even_with_a_zero_length() {
        // Given
        let m = manifest("v1", 0);

        // When
        let decision = decide(&m, "v1", SLOT);

        // Then
        assert_eq!(decision, Decision::UpToDate);
    }

    /// An empty own version means the build did not set it. The box must not
    /// treat every manifest as an update and reflash forever.
    #[test]
    fn an_empty_version_of_ours_refuses_rather_than_updating() {
        // Given
        let m = manifest("v2", 1024);

        // When
        let decision = decide(&m, "", SLOT);

        // Then
        assert_eq!(decision, Decision::Refuse(Refusal::EmptyVersion));
    }
}
