//! Operating mode state machine — spec Section 6.
//!
//! Three modes: Stop < Pause < Play (severity ordering).
//! - The human can change in any direction.
//! - The orchestrator can only lower the mode.
//! - Transitions take effect immediately.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use events::Actor;

#[derive(Debug, Error)]
#[error("only the human can raise the operating mode (attempted {from:?} -> {to:?} by {actor:?})")]
pub struct ModeTransitionError {
    pub from: Mode,
    pub to: Mode,
    pub actor: Actor,
}

/// Operating mode — spec Section 6.
///
/// Controls merge queue behavior and dispatch.
/// Severity ordering: Stop < Pause < Play.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// No new work dispatched, agents terminated, merge queue held.
    Stop = 0,
    /// Agents work normally, merge queue held, Flush available.
    Pause = 1,
    /// Fully autonomous. Orchestrator owns merge authority.
    Play = 2,
}

impl Mode {
    /// Whether new work should be dispatched in this mode.
    ///
    /// Spec Section 6.1: "No new work is dispatched" in Stop.
    /// Spec Section 6.2/6.3: Agents dispatched normally in Pause/Play.
    pub fn allows_dispatch(&self) -> bool {
        !matches!(self, Self::Stop)
    }

    /// Whether the merge queue is actively processing.
    ///
    /// Spec Section 7.2: Only Play has active merge authority.
    pub fn merge_queue_active(&self) -> bool {
        matches!(self, Self::Play)
    }

    /// Whether flush is available (spec Section 6.2).
    ///
    /// Flush pushes through approved items. Only available in Pause.
    pub fn flush_available(&self) -> bool {
        matches!(self, Self::Pause)
    }

    /// Attempt a mode transition.
    ///
    /// Spec Section 6.4:
    /// - Human can change in any direction.
    /// - Orchestrator can lower (e.g. Play -> Pause) but not raise.
    /// - System actor follows orchestrator rules.
    pub fn transition(
        &self,
        target: Mode,
        actor: &Actor,
    ) -> Result<Mode, ModeTransitionError> {
        let raising = target > *self;

        match actor {
            Actor::Human => Ok(target),
            Actor::Orchestrator | Actor::System => {
                if raising {
                    Err(ModeTransitionError {
                        from: *self,
                        to: target,
                        actor: actor.clone(),
                    })
                } else {
                    Ok(target)
                }
            }
            // Agents and scheduler cannot change the mode.
            Actor::Agent | Actor::Scheduler => Err(ModeTransitionError {
                from: *self,
                to: target,
                actor: actor.clone(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_ordering() {
        assert!(Mode::Stop < Mode::Pause);
        assert!(Mode::Pause < Mode::Play);
    }

    #[test]
    fn human_can_raise() {
        let mode = Mode::Stop;
        assert_eq!(
            mode.transition(Mode::Play, &Actor::Human).unwrap(),
            Mode::Play
        );
    }

    #[test]
    fn human_can_lower() {
        let mode = Mode::Play;
        assert_eq!(
            mode.transition(Mode::Stop, &Actor::Human).unwrap(),
            Mode::Stop
        );
    }

    #[test]
    fn orchestrator_can_lower() {
        let mode = Mode::Play;
        assert_eq!(
            mode.transition(Mode::Pause, &Actor::Orchestrator).unwrap(),
            Mode::Pause
        );
    }

    #[test]
    fn orchestrator_cannot_raise() {
        let mode = Mode::Pause;
        assert!(mode.transition(Mode::Play, &Actor::Orchestrator).is_err());
    }

    #[test]
    fn agent_cannot_change_mode() {
        let mode = Mode::Play;
        assert!(mode.transition(Mode::Pause, &Actor::Agent).is_err());
    }

    #[test]
    fn dispatch_rules() {
        assert!(!Mode::Stop.allows_dispatch());
        assert!(Mode::Pause.allows_dispatch());
        assert!(Mode::Play.allows_dispatch());
    }

    #[test]
    fn merge_queue_rules() {
        assert!(!Mode::Stop.merge_queue_active());
        assert!(!Mode::Pause.merge_queue_active());
        assert!(Mode::Play.merge_queue_active());
    }

    #[test]
    fn flush_only_in_pause() {
        assert!(!Mode::Stop.flush_available());
        assert!(Mode::Pause.flush_available());
        assert!(!Mode::Play.flush_available());
    }
}
