use crate::vcs::{VcsError, VcsService, VcsStatusSnapshot, VcsWorkspace};

/// Per-workspace refresh gate. Duplicate requests collapse to the newest generation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RefreshCoordinator {
    requested: u64,
    completed: u64,
    running: Option<u64>,
}

impl RefreshCoordinator {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            requested: 0,
            completed: 0,
            running: None,
        }
    }

    pub const fn request(&mut self) -> u64 {
        self.requested = self.requested.saturating_add(1);
        self.requested
    }

    /// Claims the newest pending generation only when no command is running.
    pub const fn start_next(&mut self) -> Option<u64> {
        if self.running.is_some() || self.requested <= self.completed {
            return None;
        }
        self.running = Some(self.requested);
        self.running
    }

    /// Releases a claimed generation when it could not be queued.
    pub fn cancel_start(&mut self, generation: u64) -> bool {
        if self.running != Some(generation) {
            return false;
        }
        self.running = None;
        true
    }

    /// Completes the active command and returns whether its result is still current.
    pub fn finish(&mut self, generation: u64) -> bool {
        if self.running != Some(generation) {
            return false;
        }
        self.running = None;
        self.completed = generation;
        generation == self.requested
    }

    #[must_use]
    pub const fn is_running(&self) -> bool {
        self.running.is_some()
    }
}

#[derive(Clone, Debug)]
pub struct RefreshResult {
    generation: u64,
    snapshot: Result<VcsStatusSnapshot, VcsError>,
}

impl RefreshResult {
    #[must_use]
    pub const fn new(generation: u64, snapshot: Result<VcsStatusSnapshot, VcsError>) -> Self {
        Self {
            generation,
            snapshot,
        }
    }

    /// Runs the claimed generation. Call this on a bounded worker, never the UI thread.
    pub fn run(generation: u64, service: &mut impl VcsService, workspace: &VcsWorkspace) -> Self {
        Self::new(generation, service.refresh_status(workspace))
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub fn into_snapshot(self) -> Result<VcsStatusSnapshot, VcsError> {
        self.snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::RefreshCoordinator;

    #[test]
    fn coalesces_requests_and_never_starts_concurrent_work() {
        let mut refresh = RefreshCoordinator::new();
        assert_eq!(refresh.request(), 1);
        assert_eq!(refresh.start_next(), Some(1));
        assert!(refresh.is_running());

        assert_eq!(refresh.request(), 2);
        assert_eq!(refresh.request(), 3);
        assert_eq!(refresh.start_next(), None);
        assert!(!refresh.finish(1), "generation one is stale");

        assert_eq!(
            refresh.start_next(),
            Some(3),
            "intermediate request is coalesced"
        );
        assert!(refresh.finish(3));
        assert!(!refresh.is_running());
        assert_eq!(refresh.start_next(), None);
    }

    #[test]
    fn rejects_completion_for_a_generation_that_is_not_running() {
        let mut refresh = RefreshCoordinator::new();
        refresh.request();
        assert_eq!(refresh.start_next(), Some(1));
        assert!(!refresh.finish(99));
        assert!(refresh.is_running());
        assert!(refresh.finish(1));
    }

    #[test]
    fn releases_a_claim_that_could_not_be_queued() {
        let mut refresh = RefreshCoordinator::new();
        refresh.request();
        assert_eq!(refresh.start_next(), Some(1));

        assert!(refresh.cancel_start(1));
        assert!(!refresh.is_running());
        assert_eq!(refresh.start_next(), Some(1));
    }
}
