//! Where a directory walk has got to.
//!
//! The walk over the SD card used to be a recursive function. Recursion cannot
//! yield to the executor without boxing its futures, and there is no allocator
//! on the device to box them in, so the walk held the single executor for as
//! long as the card was large — no heartbeat, no console, no way to stop it.
//!
//! This is the recursion unrolled into a cursor: one entry index per open
//! level, and a decision about what to do next. It knows nothing about
//! filesystems, which is what lets it be tested on the host rather than only
//! on a box with a full card in it.

/// How deep the walk descends before it refuses to go further.
///
/// A Toniebox card is `CONTENT/<8 hex>/<8 hex>`, so three is enough and four
/// leaves room. The limit exists so a directory loop is bounded rather than
/// endless, and it also fixes how many directories are open at once.
pub const MAX_DEPTH: usize = 4;

/// What sits at the cursor's current position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Found {
    /// The directory has no entry at this index; it is exhausted.
    Nothing,
    File,
    Directory,
}

/// What the caller should do about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Read the file at this position, then ask again.
    ReadFile,
    /// Open the directory at this position; the cursor has already descended.
    Descend,
    /// A directory too deep to enter. It is stepped over, not entered.
    TooDeep,
    /// Close the directory at this position; the cursor has gone back up.
    Ascend,
    /// Every level is exhausted.
    Finished,
}

/// The position of a walk in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    depth: usize,
    index: [u32; MAX_DEPTH],
}

impl Default for Cursor {
    fn default() -> Self {
        Self::new()
    }
}

impl Cursor {
    /// A walk that has just opened the root and looked at nothing.
    pub const fn new() -> Self {
        Self {
            depth: 0,
            index: [0; MAX_DEPTH],
        }
    }

    /// How many directories are open below the root.
    pub const fn depth(&self) -> usize {
        self.depth
    }

    /// Which entry of the current directory to ask about next.
    pub const fn index(&self) -> u32 {
        self.index[self.depth]
    }

    /// Steps over the current entry without acting on it.
    ///
    /// For an entry the caller could not open. Without this the walk would ask
    /// about the same unreadable entry forever.
    pub fn skip(&mut self) {
        self.index[self.depth] += 1;
    }

    /// Decides what to do about what is at the current position.
    pub fn advance(&mut self, found: Found) -> Action {
        match found {
            Found::File => {
                self.index[self.depth] += 1;
                Action::ReadFile
            }
            Found::Directory => {
                if self.depth + 1 >= MAX_DEPTH {
                    self.index[self.depth] += 1;
                    return Action::TooDeep;
                }
                self.depth += 1;
                self.index[self.depth] = 0;
                Action::Descend
            }
            Found::Nothing => {
                if self.depth == 0 {
                    return Action::Finished;
                }
                self.depth -= 1;
                // Step past the directory just left. Forgetting this is an
                // endless walk: the parent hands back the same subdirectory
                // and the cursor descends into it again, forever.
                self.index[self.depth] += 1;
                Action::Ascend
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_root_finishes_without_visiting_anything() {
        let mut cursor = Cursor::new();
        assert_eq!(cursor.advance(Found::Nothing), Action::Finished);
    }

    #[test]
    fn a_file_is_read_and_the_cursor_moves_on() {
        let mut cursor = Cursor::new();
        assert_eq!(cursor.index(), 0);
        assert_eq!(cursor.advance(Found::File), Action::ReadFile);
        assert_eq!(
            cursor.index(),
            1,
            "the next question is about the next entry"
        );
    }

    #[test]
    fn a_directory_is_descended_into_at_its_first_entry() {
        let mut cursor = Cursor::new();
        assert_eq!(cursor.advance(Found::Directory), Action::Descend);
        assert_eq!(cursor.depth(), 1);
        assert_eq!(cursor.index(), 0, "a freshly opened directory starts at 0");
    }

    /// The bug this guards is an endless walk: on leaving a subdirectory the
    /// parent must resume *past* it, or it hands back the same one forever.
    #[test]
    fn leaving_a_directory_resumes_the_parent_after_it() {
        let mut cursor = Cursor::new();
        cursor.advance(Found::Directory);
        assert_eq!(cursor.advance(Found::Nothing), Action::Ascend);
        assert_eq!(cursor.depth(), 0);
        assert_eq!(cursor.index(), 1, "not 0, or the same subdirectory repeats");
    }

    #[test]
    fn the_walk_finishes_only_when_the_root_is_exhausted() {
        let mut cursor = Cursor::new();
        cursor.advance(Found::Directory);
        assert_eq!(cursor.advance(Found::Nothing), Action::Ascend);
        assert_eq!(cursor.advance(Found::Nothing), Action::Finished);
    }

    /// Four levels is the limit, so the fourth directory down is stepped over
    /// rather than entered — and stepped over, not stuck on.
    #[test]
    fn a_directory_below_the_depth_limit_is_skipped_rather_than_entered() {
        let mut cursor = Cursor::new();
        for expected_depth in 1..MAX_DEPTH {
            assert_eq!(cursor.advance(Found::Directory), Action::Descend);
            assert_eq!(cursor.depth(), expected_depth);
        }
        assert_eq!(cursor.advance(Found::Directory), Action::TooDeep);
        assert_eq!(cursor.depth(), MAX_DEPTH - 1, "it did not descend");
        assert_eq!(cursor.index(), 1, "and it did not stall on the same entry");
    }

    #[test]
    fn an_unreadable_entry_is_stepped_over() {
        let mut cursor = Cursor::new();
        cursor.skip();
        assert_eq!(cursor.index(), 1);
    }

    /// A whole small tree, to show the order the walk visits things in:
    /// a file, then a directory holding one file, then the end.
    #[test]
    fn a_directory_between_two_files_is_walked_in_order() {
        let mut cursor = Cursor::new();
        assert_eq!(cursor.advance(Found::File), Action::ReadFile);
        assert_eq!(cursor.advance(Found::Directory), Action::Descend);
        assert_eq!(cursor.advance(Found::File), Action::ReadFile);
        assert_eq!(cursor.advance(Found::Nothing), Action::Ascend);
        assert_eq!(cursor.index(), 2, "back in the root, past the directory");
        assert_eq!(cursor.advance(Found::File), Action::ReadFile);
        assert_eq!(cursor.advance(Found::Nothing), Action::Finished);
    }

    /// Sibling directories must not inherit each other's position.
    #[test]
    fn a_second_subdirectory_starts_from_its_own_beginning() {
        let mut cursor = Cursor::new();
        cursor.advance(Found::Directory);
        cursor.advance(Found::File);
        cursor.advance(Found::File);
        assert_eq!(cursor.index(), 2);
        cursor.advance(Found::Nothing);
        assert_eq!(cursor.advance(Found::Directory), Action::Descend);
        assert_eq!(cursor.index(), 0, "the new directory starts at its own 0");
    }
}
