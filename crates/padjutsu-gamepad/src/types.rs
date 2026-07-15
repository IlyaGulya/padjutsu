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
