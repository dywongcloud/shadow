//! Lifecycle state machines for cells and jobs.

use serde::{Deserialize, Serialize};

/// The lifecycle of a single cell; legal moves are defined by [`CellState::can_transition_to`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CellState {
    /// Box daemon is booting the microVM and loading the image.
    Provisioning,
    /// Booted, image loaded, idle in the warm pool — ready for instant assignment.
    Warm,
    /// Claimed for a specific job but not yet executing.
    Assigned,
    /// Executing the customer build.
    Running,
    /// Build finished; tearing down (cells are single-use for isolation).
    Draining,
    /// Fully torn down.
    Terminated,
    /// Unrecoverable error during any phase.
    Failed,
}

impl CellState {
    pub fn is_terminal(self) -> bool {
        matches!(self, CellState::Terminated | CellState::Failed)
    }

    /// Can a cell in this state be handed to the scheduler for a new job?
    pub fn is_available(self) -> bool {
        matches!(self, CellState::Warm)
    }

    /// Validate a transition. Returns false for illegal moves so the control
    /// plane can refuse to corrupt its own bookkeeping. A `Warm` cell may drain
    /// while idle (it is reaped from the pool).
    pub fn can_transition_to(self, next: CellState) -> bool {
        use CellState::*;
        match (self, next) {
            (Provisioning, Warm)
            | (Provisioning, Failed)
            | (Warm, Assigned)
            | (Warm, Draining)
            | (Warm, Failed)
            | (Assigned, Running)
            | (Assigned, Failed)
            | (Running, Draining)
            | (Running, Failed)
            | (Draining, Terminated)
            | (Draining, Failed) => true,
            (a, b) if a == b => true,
            _ => false,
        }
    }
}

/// The lifecycle of a build job; `is_terminal` names the end states.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Scheduled,
    Building,
    Succeeded,
    Failed,
    TimedOut,
    Canceled,
}

impl JobState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            JobState::Succeeded | JobState::Failed | JobState::TimedOut | JobState::Canceled
        )
    }
}
