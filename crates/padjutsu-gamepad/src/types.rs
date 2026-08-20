/// Unique identifier of a controller or joystick device.
pub type ControllerId = u32;

/// Logical controller buttons supported by this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, padjutsu_bit_derive::Bit)]
pub enum Button {
    A,
    B,
    X,
    Y,
    Back,
    Guide,
    Start,
    LeftStick,
    RightStick,
    LeftShoulder,
    RightShoulder,
    LeftTrigger,
    RightTrigger,
    DPadUp,
    DPadDown,
    DPadLeft,
    DPadRight,
}

impl Button {
    pub const ALL: [Self; 17] = [
        Self::A,
        Self::B,
        Self::X,
        Self::Y,
        Self::Back,
        Self::Guide,
        Self::Start,
        Self::LeftStick,
        Self::RightStick,
        Self::LeftShoulder,
        Self::RightShoulder,
        Self::LeftTrigger,
        Self::RightTrigger,
        Self::DPadUp,
        Self::DPadDown,
        Self::DPadLeft,
        Self::DPadRight,
    ];

    #[inline]
    pub const fn index(self) -> usize {
        match self {
            Self::A => 0,
            Self::B => 1,
            Self::X => 2,
            Self::Y => 3,
            Self::Back => 4,
            Self::Guide => 5,
            Self::Start => 6,
            Self::LeftStick => 7,
            Self::RightStick => 8,
            Self::LeftShoulder => 9,
            Self::RightShoulder => 10,
            Self::LeftTrigger => 11,
            Self::RightTrigger => 12,
            Self::DPadUp => 13,
            Self::DPadDown => 14,
            Self::DPadLeft => 15,
            Self::DPadRight => 16,
        }
    }
}

/// Latest authoritative pressed state for every logical controller button.
pub type ButtonSnapshot = [bool; Button::ALL.len()];

/// Analog axes supported by this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Axis {
    LeftX,
    LeftY,
    RightX,
    RightY,
    LeftTrigger,
    RightTrigger,
}

impl Axis {
    pub const ALL: [Self; 6] = [
        Self::LeftX,
        Self::LeftY,
        Self::RightX,
        Self::RightY,
        Self::LeftTrigger,
        Self::RightTrigger,
    ];

    #[inline]
    pub const fn index(self) -> usize {
        match self {
            Self::LeftX => 0,
            Self::LeftY => 1,
            Self::RightX => 2,
            Self::RightY => 3,
            Self::LeftTrigger => 4,
            Self::RightTrigger => 5,
        }
    }
}

/// Latest authoritative values for all controller axes.
pub type AxisSnapshot = [f32; Axis::ALL.len()];

/// Controller meta information that remains stable across events.
#[derive(Debug, Clone)]
pub struct ControllerInfo {
    pub id: ControllerId,
    pub name: String,
    pub supports_rumble: bool,
    pub vendor_id: u16,
    pub product_id: u16,
}
