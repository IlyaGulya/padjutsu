use padjutsu_gamepad::ControllerId;
use padjutsu_workspace::{StickMode, StickSide};

use crate::app::Padjutsu;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StickActivity {
    Neutral,
    Active,
    Repeating,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StickState {
    pub side: StickSide,
    pub mode: Option<StickModeKind>,
    pub activity: StickActivity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StickModeKind {
    Arrows,
    Volume,
    Brightness,
    MouseMove,
    Scroll,
    TrackpadScroll,
}

impl StickModeKind {
    fn from_mode(mode: &StickMode) -> Self {
        match mode {
            StickMode::Arrows(_) => Self::Arrows,
            StickMode::Volume(_) => Self::Volume,
            StickMode::Brightness(_) => Self::Brightness,
            StickMode::MouseMove(_) => Self::MouseMove,
            StickMode::Scroll(_) => Self::Scroll,
            StickMode::TrackpadScroll(_) => Self::TrackpadScroll,
        }
    }
}

pub fn resolve_stick_state(
    padjutsu: &Padjutsu,
    controller: ControllerId,
    side: StickSide,
) -> StickState {
    let activity = if padjutsu.controller_stick_has_repeats(controller, side) {
        StickActivity::Repeating
    } else if padjutsu.controller_stick_has_axis_activity(controller, side, 0.05) {
        StickActivity::Active
    } else {
        StickActivity::Neutral
    };

    let mode = padjutsu
        .controller_stick_mode(controller, side)
        .map(StickModeKind::from_mode);

    StickState {
        side,
        mode,
        activity,
    }
}

pub fn stick_transition(
    previous: Option<StickState>,
    next: StickState,
) -> Option<(Option<StickState>, StickState)> {
    if previous == Some(next) {
        None
    } else {
        Some((previous, next))
    }
}
