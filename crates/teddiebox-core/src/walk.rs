//! The position of a directory walk over the SD card.
//!
//! A recursive async walk would need boxed futures, and the device has no
//! allocator. Without yielding, a walk over a large card would block the
//! only executor. So the walk is a cursor instead: one entry index per open
//! directory level, and a decision about what to do next.
//!
//! It knows nothing about filesystems, so it can be tested on the host.

/// How deep the walk descends before it refuses to go further.
///
/// A Toniebox card uses `CONTENT/<8 hex>/<8 hex>`, so three levels are enough
/// and four leave room. The limit stops a directory loop from being endless,
/// and bounds how many directories are open at once.
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
    /// For an entry the caller could not open. Without this, the walk would
    /// ask about the same entry forever.
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
                // Step past the directory just left, or the walk would enter
                // it again forever.
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
        // Given
        let mut cursor = Cursor::new();

        // When
        let action = cursor.advance(Found::Nothing);

        // Then
        assert_eq!(action, Action::Finished);
    }

    #[test]
    fn a_file_is_read_and_the_cursor_moves_on() {
        // Given
        let mut cursor = Cursor::new();
        assert_eq!(cursor.index(), 0);

        // When
        let action = cursor.advance(Found::File);

        // Then
        assert_eq!(action, Action::ReadFile);
        assert_eq!(
            cursor.index(),
            1,
            "the next question is about the next entry"
        );
    }

    #[test]
    fn a_directory_is_descended_into_at_its_first_entry() {
        // Given
        let mut cursor = Cursor::new();

        // When
        let action = cursor.advance(Found::Directory);

        // Then
        assert_eq!(action, Action::Descend);
        assert_eq!(cursor.depth(), 1);
        assert_eq!(cursor.index(), 0, "a freshly opened directory starts at 0");
    }

    /// On leaving a subdirectory, the parent must continue *after* it, or the
    /// walk never ends.
    #[test]
    fn leaving_a_directory_resumes_the_parent_after_it() {
        // Given
        let mut cursor = Cursor::new();
        cursor.advance(Found::Directory);

        // When
        let action = cursor.advance(Found::Nothing);

        // Then
        assert_eq!(action, Action::Ascend);
        assert_eq!(cursor.depth(), 0);
        assert_eq!(cursor.index(), 1, "not 0, or the same subdirectory repeats");
    }

    #[test]
    fn the_walk_finishes_only_when_the_root_is_exhausted() {
        // Given
        let mut cursor = Cursor::new();
        cursor.advance(Found::Directory);

        // When
        let actions = [Found::Nothing; 2].map(|found| cursor.advance(found));

        // Then
        assert_eq!(actions, [Action::Ascend, Action::Finished]);
    }

    /// Descends to the deepest allowed level, so the next directory must be
    /// refused.
    fn at_the_deepest_allowed_level() -> Cursor {
        let mut cursor = Cursor::new();
        while cursor.depth() < MAX_DEPTH - 1 {
            cursor.advance(Found::Directory);
        }
        cursor
    }

    #[test]
    fn descending_stops_at_the_depth_limit() {
        // Given
        let mut cursor = at_the_deepest_allowed_level();

        // When
        let action = cursor.advance(Found::Directory);

        // Then
        assert_eq!(action, Action::TooDeep);
    }

    #[test]
    fn a_directory_too_deep_to_enter_is_not_entered() {
        // Given
        let mut cursor = at_the_deepest_allowed_level();

        // When
        cursor.advance(Found::Directory);

        // Then
        assert_eq!(cursor.depth(), MAX_DEPTH - 1);
    }

    /// Refusing to descend must still step past the entry, or the walk never
    /// ends.
    #[test]
    fn a_directory_too_deep_to_enter_is_still_stepped_over() {
        // Given
        let mut cursor = at_the_deepest_allowed_level();
        let before = cursor.index();

        // When
        cursor.advance(Found::Directory);

        // Then
        assert_eq!(cursor.index(), before + 1);
    }

    #[test]
    fn every_level_above_the_limit_is_descended_into() {
        // Given
        let mut cursor = Cursor::new();

        // When
        let steps: [(Action, usize); MAX_DEPTH - 1] =
            core::array::from_fn(|_| (cursor.advance(Found::Directory), cursor.depth()));

        // Then
        assert_eq!(
            steps,
            [
                (Action::Descend, 1),
                (Action::Descend, 2),
                (Action::Descend, 3)
            ]
        );
    }

    #[test]
    fn an_unreadable_entry_is_stepped_over() {
        // Given
        let mut cursor = Cursor::new();

        // When
        cursor.skip();

        // Then
        assert_eq!(cursor.index(), 1);
    }

    /// A whole small tree, to show the order the walk visits things in:
    /// a file, then a directory holding one file, then the end.
    #[test]
    fn a_directory_between_two_files_is_walked_in_order() {
        // Given
        let mut cursor = Cursor::new();
        let tree = [
            Found::File,
            Found::Directory,
            Found::File,
            Found::Nothing,
            Found::File,
            Found::Nothing,
        ];

        // When
        let actions = tree.map(|found| cursor.advance(found));

        // Then
        assert_eq!(
            actions,
            [
                Action::ReadFile,
                Action::Descend,
                Action::ReadFile,
                Action::Ascend,
                Action::ReadFile,
                Action::Finished,
            ]
        );
    }

    /// Sibling directories must not inherit each other's position.
    #[test]
    fn a_second_subdirectory_starts_from_its_own_beginning() {
        // Given: a first subdirectory walked two entries in, then left
        let mut cursor = Cursor::new();
        cursor.advance(Found::Directory);
        cursor.advance(Found::File);
        cursor.advance(Found::File);
        assert_eq!(cursor.index(), 2);
        cursor.advance(Found::Nothing);

        // When
        let action = cursor.advance(Found::Directory);

        // Then
        assert_eq!(action, Action::Descend);
        assert_eq!(cursor.index(), 0, "the new directory starts at its own 0");
    }
}
