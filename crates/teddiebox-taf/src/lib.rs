#![no_std]

/// Every structure in a Tonie audio file is aligned to this boundary: the
/// header occupies page 0, and each Ogg page occupies exactly one page
/// thereafter. This is what makes page-indexed I/O possible on device.
pub const PAGE_SIZE: usize = 4096;

/// Upper bound on chapters in one file. Fixed because there is no allocator.
pub const MAX_CHAPTERS: usize = 100;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_size_is_four_kilobytes() {
        assert_eq!(PAGE_SIZE, 4096);
    }
}
