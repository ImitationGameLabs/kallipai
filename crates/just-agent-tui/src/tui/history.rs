//! Input history buffer with up/down navigation.

/// Stores submitted input lines and supports cursor-based navigation.
pub struct InputHistory {
    entries: Vec<String>,
    /// Index into entries when navigating; `entries.len()` means "current draft".
    position: usize,
    /// Saved draft text before the user started navigating history.
    draft: Option<String>,
}

impl InputHistory {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            position: 0,
            draft: None,
        }
    }

    /// Record a submitted input. Resets navigation position.
    pub fn push(&mut self, input: String) {
        if !input.is_empty() {
            self.entries.push(input);
        }
        self.position = self.entries.len();
        self.draft = None;
    }

    /// Go to the previous (older) entry. Saves `current` as draft on first call.
    /// Returns the history entry, or `None` if already at the oldest.
    pub fn up(&mut self, current: &str) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }
        if self.position == self.entries.len() {
            // First press of Up — save whatever the user was typing
            self.draft = Some(current.to_owned());
        }
        if self.position > 0 {
            self.position -= 1;
        }
        Some(&self.entries[self.position])
    }

    /// Go to the next (newer) entry. Returns the saved draft when reaching the end.
    /// Returns `None` if already at the newest.
    pub fn down(&mut self) -> Option<Either<'_>> {
        if self.position >= self.entries.len() {
            return None;
        }
        self.position += 1;
        if self.position == self.entries.len() {
            self.draft.take().map(Either::Draft)
        } else {
            Some(Either::Entry(&self.entries[self.position]))
        }
    }

    /// Reset navigation position to newest.
    #[allow(dead_code)]
    pub fn reset(&mut self) {
        self.position = self.entries.len();
        self.draft = None;
    }
}

/// Result of navigating down past the newest history entry.
pub enum Either<'a> {
    Entry(&'a str),
    Draft(String),
}
