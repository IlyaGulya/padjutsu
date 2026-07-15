use enigo::{Axis, Button, Direction, Enigo, InputResult, Mouse, NewConError, Settings};
#[cfg(not(target_os = "macos"))]
use enigo::Coordinate;

use crate::KeyCombo;

#[derive(Debug, Clone, Copy)]
pub(crate) struct MouseMoveObservation {
    pub(crate) x: i32,
    pub(crate) y: i32,
}

/// Wrap a CG/Cocoa-using closure in a macOS autorelease pool so any
/// internally-allocated CFData/NSObject autoreleased values are freed
/// at the end of the call. Without a pool, those values accumulate in
/// the absence of a Cocoa main run loop and look like a memory leak.
#[cfg(target_os = "macos")]
#[inline]
fn with_pool<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    objc2::rc::autoreleasepool(|_| f())
}

#[cfg(not(target_os = "macos"))]
#[inline]
fn with_pool<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    f()
}

#[cfg(target_os = "macos")]
mod cg_source {
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    use std::cell::RefCell;

    thread_local! {
        static SOURCE: RefCell<Option<CGEventSource>> = const { RefCell::new(None) };
    }

    /// Run a closure with a cached, thread-local CGEventSource.
    /// The source is created once per thread and reused for all events,
    /// avoiding the per-event CFData allocations that would otherwise leak
    /// or churn through the system allocator.
    pub fn with<F, R>(f: F) -> Result<R, &'static str>
    where
        F: FnOnce(&CGEventSource) -> R,
    {
        SOURCE.with(|cell| {
            let mut slot = cell.borrow_mut();
            if slot.is_none() {
                let src =
                    CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
                        .map_err(|_| "failed to create CGEventSource")?;
                *slot = Some(src);
            }
            Ok(f(slot.as_ref().expect("CGEventSource initialized above")))
        })
    }
}

#[cfg(target_os = "macos")]
mod relative_mouse {
    use core_graphics::{
        display::CGPoint,
        event::{
            CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton,
            EventField,
        },
    };
    use enigo::{InputError, InputResult};
    use objc2_app_kit::NSEvent;

    use super::MouseMoveObservation;

    /// Mouse moves must remain coalescible. Under WindowServer load,
    /// non-coalesced events form a downstream queue that can keep moving the
    /// cursor after the stick has already returned to neutral.
    #[inline]
    fn movement_event_flags() -> CGEventFlags {
        CGEventFlags::empty()
    }

    /// Post a relative move while preserving Enigo's macOS semantics.
    ///
    /// The live cursor position and pressed buttons are intentionally queried
    /// for every event. Only the display height is cached by `Performer`;
    /// querying it through `CGDisplayPixelsHigh` on every tick can block on
    /// WindowServer for milliseconds under graphics load.
    pub fn post(
        dx: i32,
        dy: i32,
        display_height: i32,
    ) -> InputResult<MouseMoveObservation> {
        let pressed = unsafe { NSEvent::pressedMouseButtons() };
        let point = unsafe { NSEvent::mouseLocation() };
        let current_x = point.x as i32;
        let current_y = display_height - point.y as i32;

        let (event_type, button) = if pressed & 1 > 0 {
            (CGEventType::LeftMouseDragged, CGMouseButton::Left)
        } else if pressed & 2 > 0 {
            (CGEventType::RightMouseDragged, CGMouseButton::Right)
        } else {
            (CGEventType::MouseMoved, CGMouseButton::Left)
        };

        let destination = CGPoint::new(
            f64::from(current_x.saturating_add(dx)),
            f64::from(current_y.saturating_add(dy)),
        );
        let event = super::cg_source::with(|source| {
            CGEvent::new_mouse_event(source.clone(), event_type, destination, button)
        })
        .map_err(|_| InputError::Simulate("failed to create mouse source"))?
        .map_err(|_| InputError::Simulate("failed creating relative mouse event"))?;

        event
            .set_integer_value_field(EventField::MOUSE_EVENT_DELTA_X, i64::from(dx));
        event
            .set_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y, i64::from(dy));
        event.set_integer_value_field(
            EventField::EVENT_SOURCE_USER_DATA,
            enigo::EVENT_MARKER as i64,
        );
        event.set_flags(movement_event_flags());
        event.post(CGEventTapLocation::HID);
        Ok(MouseMoveObservation {
            x: current_x,
            y: current_y,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn movement_events_allow_window_server_coalescing() {
            let flags = movement_event_flags();
            assert!(!flags.contains(CGEventFlags::CGEventFlagNonCoalesced));
            assert_eq!(flags.bits() & 0x2000_0000, 0);
        }
    }
}

#[cfg(target_os = "macos")]
mod raw_modifier {
    use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation, CGEventType};

    /// Device-specific flag bits (from IOKit NX headers).
    const NX_DEVICELCTLKEYMASK: u64 = 0x0000_0001;
    const NX_DEVICERCTLKEYMASK: u64 = 0x0000_2000;
    const NX_DEVICELSHIFTKEYMASK: u64 = 0x0000_0002;
    const NX_DEVICERSHIFTKEYMASK: u64 = 0x0000_0004;
    const NX_DEVICELCMDKEYMASK: u64 = 0x0000_0008;
    const NX_DEVICERCMDKEYMASK: u64 = 0x0000_0010;
    const NX_DEVICELALTKEYMASK: u64 = 0x0000_0020;
    const NX_DEVICERALTKEYMASK: u64 = 0x0000_0040;

    /// macOS virtual keycodes for modifier keys.
    pub const KC_CONTROL: u16 = 0x3B;
    pub const KC_RIGHT_CONTROL: u16 = 0x3E;
    pub const KC_SHIFT: u16 = 0x38;
    pub const KC_RIGHT_SHIFT: u16 = 0x3C;
    pub const KC_COMMAND: u16 = 0x37;
    pub const KC_RIGHT_COMMAND: u16 = 0x36;
    pub const KC_OPTION: u16 = 0x3A;
    pub const KC_RIGHT_OPTION: u16 = 0x3D;

    /// Returns (high-level CGEventFlags mask, device-specific mask) for a modifier keycode.
    fn modifier_flags(keycode: u16) -> Option<(CGEventFlags, u64)> {
        match keycode {
            KC_CONTROL => {
                Some((CGEventFlags::CGEventFlagControl, NX_DEVICELCTLKEYMASK))
            }
            KC_RIGHT_CONTROL => {
                Some((CGEventFlags::CGEventFlagControl, NX_DEVICERCTLKEYMASK))
            }
            KC_SHIFT => {
                Some((CGEventFlags::CGEventFlagShift, NX_DEVICELSHIFTKEYMASK))
            }
            KC_RIGHT_SHIFT => {
                Some((CGEventFlags::CGEventFlagShift, NX_DEVICERSHIFTKEYMASK))
            }
            KC_COMMAND => {
                Some((CGEventFlags::CGEventFlagCommand, NX_DEVICELCMDKEYMASK))
            }
            KC_RIGHT_COMMAND => {
                Some((CGEventFlags::CGEventFlagCommand, NX_DEVICERCMDKEYMASK))
            }
            KC_OPTION => {
                Some((CGEventFlags::CGEventFlagAlternate, NX_DEVICELALTKEYMASK))
            }
            KC_RIGHT_OPTION => {
                Some((CGEventFlags::CGEventFlagAlternate, NX_DEVICERALTKEYMASK))
            }
            _ => None,
        }
    }

    /// Post a FlagsChanged CGEvent, which is what macOS generates for real modifier keypresses.
    pub fn post_flags_changed(keycode: u16, pressed: bool) -> Result<(), String> {
        let (high_flag, dev_flag) = modifier_flags(keycode)
            .ok_or_else(|| format!("keycode 0x{keycode:02x} is not a modifier"))?;

        // Use cached thread-local CGEventSource so we don't allocate a new
        // one per event (CFData leak / churn).
        let event = super::cg_source::with(|source| {
            CGEvent::new_keyboard_event(source.clone(), keycode, pressed)
        })?
        .map_err(|_| "failed to create CGEvent")?;

        // Override event type to FlagsChanged (type 12).
        event.set_type(CGEventType::FlagsChanged);

        // Build the flags bitfield.
        let mut flags = CGEventFlags::CGEventFlagNonCoalesced;
        if pressed {
            flags.insert(high_flag);
            flags.insert(CGEventFlags::from_bits_retain(dev_flag));
        }
        // When releasing, flags should be empty (no modifier held).
        event.set_flags(flags);

        log::info!(
            "[raw_modifier] posting FlagsChanged keycode=0x{keycode:02x} pressed={pressed} flags=0x{:016x}",
            flags.bits()
        );

        // Post at HID level so the event goes through the full macOS input pipeline.
        // Using CombinedSessionState source ensures the global modifier state is updated.
        event.post(CGEventTapLocation::HID);
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod smooth_scroll {
    use core_graphics::event::{
        CGEvent, CGEventTapLocation, EventField, ScrollEventUnit,
    };

    use enigo::{Axis, InputError, InputResult};
    use log::debug;

    pub fn post(axis: Axis, value: f64) -> InputResult<()> {
        // Use cached thread-local CGEventSource (see `cg_source` module above)
        // to avoid allocating a fresh source per event.
        let event = super::cg_source::with(|source| {
            CGEvent::new_scroll_event(
                source.clone(),
                ScrollEventUnit::PIXEL,
                2,
                0,
                0,
                0,
            )
        })
        .map_err(|_| InputError::Simulate("failed to create scroll source"))?
        .map_err(|_| InputError::Simulate("failed creating smooth scroll event"))?;

        let fixed_value = (value * 65536.0).round() as i64;
        let point_value = value.round() as i64;
        let (
            delta_axis_1,
            delta_axis_2,
            fixed_axis_1,
            fixed_axis_2,
            point_axis_1,
            point_axis_2,
        ) = match axis {
            Axis::Vertical => (0, 0, fixed_value, 0, point_value, 0),
            Axis::Horizontal => (0, 0, 0, fixed_value, 0, point_value),
        };

        debug!(
            "[smooth_scroll] axis={axis:?} input={value:.3} delta1={} delta2={} fixed1={} fixed2={} point1={} point2={}",
            delta_axis_1,
            delta_axis_2,
            fixed_axis_1,
            fixed_axis_2,
            point_axis_1,
            point_axis_2
        );

        event.set_integer_value_field(
            EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1,
            delta_axis_1,
        );
        event.set_integer_value_field(
            EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2,
            delta_axis_2,
        );
        event.set_integer_value_field(
            EventField::SCROLL_WHEEL_EVENT_FIXED_POINT_DELTA_AXIS_1,
            fixed_axis_1,
        );
        event.set_integer_value_field(
            EventField::SCROLL_WHEEL_EVENT_FIXED_POINT_DELTA_AXIS_2,
            fixed_axis_2,
        );
        event.set_integer_value_field(
            EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1,
            point_axis_1,
        );
        event.set_integer_value_field(
            EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_2,
            point_axis_2,
        );
        event.post(CGEventTapLocation::HID);
        debug!("[smooth_scroll] posted axis={axis:?} input={value:.3}");
        Ok(())
    }
}

pub struct Performer {
    enigo: Enigo,
    #[cfg(target_os = "macos")]
    display_height: i32,
}

// SAFETY: This is safe because we're only accessing Enigo through a Mutex,
// which provides the necessary synchronization. The internal CGEventSource
// is only used on the thread that actually performs the key presses.
unsafe impl Send for Performer {}
unsafe impl Sync for Performer {}

impl Performer {
    /// Create a new performer.
    pub fn new() -> Result<Self, NewConError> {
        let settings = Settings::default();
        let enigo = Enigo::new(&settings)?;
        #[cfg(target_os = "macos")]
        let display_height =
            core_graphics::display::CGDisplay::main().pixels_high() as i32;
        Ok(Self {
            enigo,
            #[cfg(target_os = "macos")]
            display_height,
        })
    }

    /// Perform key combo.
    /// This will press and release the keys in the key combo.
    pub fn perform(&mut self, key_combo: &KeyCombo) -> InputResult<()> {
        with_pool(|| key_combo.perform(&mut self.enigo))
    }

    /// Press keys.
    pub fn press(&mut self, key_combo: &KeyCombo) -> InputResult<()> {
        with_pool(|| key_combo.press(&mut self.enigo))
    }

    /// Release keys.
    pub fn release(&mut self, key_combo: &KeyCombo) -> InputResult<()> {
        with_pool(|| key_combo.release(&mut self.enigo))
    }

    /// Move mouse.
    #[cfg(target_os = "macos")]
    pub fn mouse_move(&mut self, x: i32, y: i32) -> InputResult<()> {
        self.mouse_move_observed(x, y).map(|_| ())
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn mouse_move_observed(
        &mut self,
        x: i32,
        y: i32,
    ) -> InputResult<Option<MouseMoveObservation>> {
        with_pool(|| relative_mouse::post(x, y, self.display_height)).map(Some)
    }

    /// Fallback for non-macOS systems.
    #[cfg(not(target_os = "macos"))]
    pub fn mouse_move(&mut self, x: i32, y: i32) -> InputResult<()> {
        with_pool(|| self.enigo.move_mouse(x, y, Coordinate::Rel))
    }

    #[cfg(not(target_os = "macos"))]
    pub(crate) fn mouse_move_observed(
        &mut self,
        x: i32,
        y: i32,
    ) -> InputResult<Option<MouseMoveObservation>> {
        self.mouse_move(x, y).map(|_| None)
    }

    /// Scroll horizontally.
    /// Uses macOS specific smooth scrolling.
    #[cfg(target_os = "macos")]
    pub fn scroll_x(&mut self, value: f64) -> InputResult<()> {
        with_pool(|| smooth_scroll::post(Axis::Horizontal, value))
    }

    /// Scroll vertically.
    /// Uses macOS specific smooth scrolling.
    #[cfg(target_os = "macos")]
    pub fn scroll_y(&mut self, value: f64) -> InputResult<()> {
        with_pool(|| smooth_scroll::post(Axis::Vertical, value))
    }

    /// Fallback for non-macOS systems
    #[cfg(not(target_os = "macos"))]
    pub fn scroll_x(&mut self, value: f64) -> InputResult<()> {
        self.enigo.scroll(value.round() as i32, Axis::Horizontal)
    }

    #[cfg(not(target_os = "macos"))]
    pub fn scroll_y(&mut self, value: f64) -> InputResult<()> {
        self.enigo.scroll(value.round() as i32, Axis::Vertical)
    }

    /// Click a mouse button.
    pub fn mouse_click(&mut self, button: Button) -> InputResult<()> {
        with_pool(|| self.enigo.button(button, Direction::Click))
    }

    /// Double-click a mouse button.
    pub fn mouse_double_click(&mut self, button: Button) -> InputResult<()> {
        with_pool(|| {
            self.enigo.button(button, Direction::Click)?;
            self.enigo.button(button, Direction::Click)
        })
    }

    /// Press a mouse button (hold down).
    pub fn mouse_press(&mut self, button: Button) -> InputResult<()> {
        with_pool(|| self.enigo.button(button, Direction::Press))
    }

    /// Release a mouse button.
    pub fn mouse_release(&mut self, button: Button) -> InputResult<()> {
        with_pool(|| self.enigo.button(button, Direction::Release))
    }

    /// Send a raw modifier key press via FlagsChanged CGEvent (macOS only).
    /// This is the correct event type for modifier keys — apps like Freeflow
    /// and SuperWhisper that listen for modifier-only keypresses need this.
    #[cfg(target_os = "macos")]
    pub fn raw_modifier_press(&mut self, keycode: u16) -> Result<(), String> {
        with_pool(|| raw_modifier::post_flags_changed(keycode, true))
    }

    /// Send a raw modifier key release via FlagsChanged CGEvent (macOS only).
    #[cfg(target_os = "macos")]
    pub fn raw_modifier_release(&mut self, keycode: u16) -> Result<(), String> {
        with_pool(|| raw_modifier::post_flags_changed(keycode, false))
    }
}
