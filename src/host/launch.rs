use super::PaneId;

/// Current dock visibility relative to focused pane.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DockState {
    Absent,
    Present {
        dock_pane_id: PaneId,
        focused_pane_id: PaneId,
    },
}

/// Side-effect-free action selected by dock toggle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToggleDecision {
    Open,
    Focus { pane_id: PaneId },
    Close { pane_id: PaneId },
}

/// Decides future toggle behavior without calling Herdr.
///
/// Invariant: absent docks open, unfocused docks focus, and focused docks close.
#[must_use]
pub fn decide_toggle(state: DockState) -> ToggleDecision {
    match state {
        DockState::Absent => ToggleDecision::Open,
        DockState::Present {
            dock_pane_id,
            focused_pane_id,
        } if dock_pane_id == focused_pane_id => ToggleDecision::Close {
            pane_id: dock_pane_id,
        },
        DockState::Present { dock_pane_id, .. } => ToggleDecision::Focus {
            pane_id: dock_pane_id,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{DockState, ToggleDecision, decide_toggle};
    use crate::host::PaneId;

    #[test]
    fn absent_dock_opens() {
        assert_eq!(decide_toggle(DockState::Absent), ToggleDecision::Open);
    }

    #[test]
    fn unfocused_dock_receives_focus() -> Result<(), crate::host::LaunchContextError> {
        let dock = PaneId::new("dock")?;
        let terminal = PaneId::new("terminal")?;

        assert_eq!(
            decide_toggle(DockState::Present {
                dock_pane_id: dock.clone(),
                focused_pane_id: terminal,
            }),
            ToggleDecision::Focus { pane_id: dock }
        );
        Ok(())
    }

    #[test]
    fn focused_dock_closes() -> Result<(), crate::host::LaunchContextError> {
        let dock = PaneId::new("dock")?;

        assert_eq!(
            decide_toggle(DockState::Present {
                dock_pane_id: dock.clone(),
                focused_pane_id: dock.clone(),
            }),
            ToggleDecision::Close { pane_id: dock }
        );
        Ok(())
    }
}
