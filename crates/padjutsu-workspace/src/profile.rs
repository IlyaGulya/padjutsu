use std::sync::Arc;
use core::str;
use ahash::{AHashMap, AHashSet};

use padjutsu_control::KeyCombo;
use padjutsu_gamepad::Button;
use smallvec::SmallVec;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProfileError {
    #[error("yaml deserialize error: {0}")]
    YamlDeserializeError(#[from] serde_yaml::Error),
    #[error("unsupported version: {0}")]
    UnsupportedVersion(u8),
    #[error("v1 profile error: {0}")]
    V1Profile(#[from] v1::Error),
}

use crate::{v1, BundleId, ButtonChord, ControllerId};

/// A set of rules to handle button presses for an app.
pub type ButtonRules = AHashMap<ButtonChord, ButtonRule>;

/// A set of rules to handle stick movements for an app.
pub type StickRules = AHashMap<StickSide, StickMode>;

/// Profile is a collection of rules and settings for controllers and applications.
#[derive(Debug, Clone)]
pub struct Profile {
    /// Controller settings.
    pub controllers: ControllerSettingsMap,
    /// Blacklist apps.
    pub blacklist: AHashSet<String>,
    /// App rules.
    pub rules: RuleMap,
    /// Shell to run for shell actions.
    pub shell: Option<Box<str>>,
}

/// A set of rules to handle controller settings for an app.
#[derive(Debug, Clone, Default)]
pub struct AppRules {
    pub buttons: ButtonRules,
    pub sticks: StickRules,
}

/// Controller parameters.
#[derive(Debug, Clone, Default)]
pub struct ControllerSettings {
    pub mapping: AHashMap<Button, Button>,
}

impl ControllerSettings {
    pub fn new(mapping: AHashMap<Button, Button>) -> Self {
        Self { mapping }
    }
}

/// A set of rules to handle app settings for an app.
pub type RuleMap = AHashMap<BundleId, AppRules>;

/// A set of rules to handle app settings for an app.
pub type ControllerSettingsMap = AHashMap<ControllerId, ControllerSettings>;

/// A set of macros.
pub type Macros = SmallVec<[KeyCombo; 4]>;

/// A mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// A mouse click type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseClickType {
    Click,
    DoubleClick,
}

/// A raw modifier key identifier (macOS virtual keycode).
/// Used for sending FlagsChanged events that apps like Freeflow/SuperWhisper expect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawModifierKey {
    Control,
    RControl,
    Shift,
    RShift,
    Command,
    RCommand,
    Option,
    ROption,
}

impl RawModifierKey {
    /// Return the macOS virtual keycode for this modifier.
    #[cfg(target_os = "macos")]
    pub fn keycode(self) -> u16 {
        match self {
            Self::Control => 0x3B,
            Self::RControl => 0x3E,
            Self::Shift => 0x38,
            Self::RShift => 0x3C,
            Self::Command => 0x37,
            Self::RCommand => 0x36,
            Self::Option => 0x3A,
            Self::ROption => 0x3D,
        }
    }
}

/// A action for a gamepad button.
#[derive(Debug, Clone)]
pub enum ButtonAction {
    /// Press+release on button press, then optionally repeat while held.
    Keystroke(Arc<KeyCombo>),
    /// Hold: press on button down, release on button up.
    HoldKeystroke(Arc<KeyCombo>),
    /// Tap: press+release immediately on button press. No key repeat. Use `tap:`.
    TapKeystroke(Arc<KeyCombo>),
    Macros(Arc<Macros>),
    Shell(String),
    MouseClick {
        button: MouseButton,
        click_type: MouseClickType,
    },
    /// Hold: press mouse button on gamepad down, release on gamepad up. For drag operations.
    HoldClick(MouseButton),
    /// Send a raw modifier key as a FlagsChanged CGEvent (macOS).
    /// This is needed for apps that listen for modifier-only keypresses.
    RawModifier(RawModifierKey),
}

/// A rule for a gamepad button.
#[derive(Debug, Clone)]
pub struct ButtonRule {
    pub action: ButtonAction,
    pub vibrate: Option<u16>,
    pub repeat_delay_ms: Option<u64>,
    pub repeat_interval_ms: Option<u64>,
}

/// A side of a stick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StickSide {
    Left,
    Right,
}

/// An axis of a stick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Axis {
    X,
    Y,
}

/// A mode of a gamepad stick.
#[derive(Debug, Clone)]
pub enum StickMode {
    Arrows(ArrowsParams),
    Volume(StepperParams),
    Brightness(StepperParams),
    MouseMove(MouseParams),
    Scroll(ScrollParams),
    TrackpadScroll(ScrollParams),
}

/// Parameters for the arrows mode.
#[derive(Debug, Clone)]
pub struct ArrowsParams {
    pub deadzone: f32,
    pub repeat_delay_ms: u64,
    pub repeat_interval_ms: u64,
    pub invert_x: bool,
    pub invert_y: bool,
}

/// Parameters for the volume/brightness modes.
#[derive(Debug, Clone)]
pub struct StepperParams {
    pub axis: Axis,
    pub deadzone: f32,
    pub min_interval_ms: u64,
    pub max_interval_ms: u64,
    pub invert: bool,
}

/// Parameters for the mouse move mode.
#[derive(Debug, Clone)]
pub struct MouseParams {
    pub deadzone: f32,
    pub outer_deadzone: f32,
    pub max_speed_px_s: f32,
    pub gamma: f32,
    pub precision_multiplier: f32,
    pub precision_button: Option<padjutsu_gamepad::Button>,
    pub invert_x: bool,
    pub invert_y: bool,
    pub runtime: MouseRuntimeParams,
}

/// Runtime tuning parameters for mouse move mode.
#[derive(Debug, Clone)]
pub struct MouseRuntimeParams {
    pub tick_ms: u64,
    pub smoothing_window_ms: u64,
}

/// Parameters for the scroll mode.
#[derive(Debug, Clone)]
pub struct ScrollParams {
    pub deadzone: f32,
    pub speed_lines_s: f32,
    pub horizontal: bool,
    /// When true, locks to the dominant axis once scrolling starts.
    /// Unlocks when the stick returns to the deadzone.
    pub axis_lock: bool,
    pub invert_x: bool,
    pub invert_y: bool,
    /// Optional button that turns trackpad scrolling into Cmd+scroll zoom.
    pub zoom_button: Option<padjutsu_gamepad::Button>,
    pub runtime: ScrollRuntimeParams,
}

/// Runtime tuning parameters for scroll mode.
#[derive(Debug, Clone)]
pub struct ScrollRuntimeParams {
    pub tick_ms: u64,
    pub smoothing_window_ms: u64,
    pub gamma: f32,
    pub trigger_boost_max: f32,
    pub trigger_boost_gamma: f32,
}
