use enigo::{Axis, Button, Enigo, InputResult, NewConError, Settings};
#[cfg(not(target_os = "macos"))]
use enigo::{Coordinate, Direction, Mouse};

use crate::KeyCombo;

#[derive(Debug, Clone, Copy)]
pub(crate) struct MouseMoveObservation {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) posted_dx: i32,
    pub(crate) posted_dy: i32,
    pub(crate) display_epoch: u64,
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
mod display_configuration {
    use std::ffi::c_void;
    use std::ptr;
    use std::sync::atomic::{AtomicU64, Ordering};

    use core_graphics::display::{
        CGDisplayChangeSummaryFlags, CGDisplayRegisterReconfigurationCallback,
        CGDisplayRemoveReconfigurationCallback,
    };

    static EPOCH: AtomicU64 = AtomicU64::new(0);

    unsafe extern "C" fn reconfigured(
        _display: u32,
        flags: u32,
        _user_info: *const c_void,
    ) {
        // CoreGraphics invokes the callback both before and after a change.
        // Publish only completed configurations.
        if flags
            & CGDisplayChangeSummaryFlags::kCGDisplayBeginConfigurationFlag.bits()
            == 0
        {
            EPOCH.fetch_add(1, Ordering::Release);
        }
    }

    pub fn epoch() -> u64 {
        EPOCH.load(Ordering::Acquire)
    }

    pub struct Observer;

    impl Observer {
        pub fn register() -> Option<Self> {
            let result = unsafe {
                CGDisplayRegisterReconfigurationCallback(reconfigured, ptr::null())
            };
            (result == 0).then_some(Self)
        }
    }

    impl Drop for Observer {
        fn drop(&mut self) {
            unsafe {
                CGDisplayRemoveReconfigurationCallback(reconfigured, ptr::null());
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod relative_mouse {
    use std::time::{Duration, Instant};

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

    const MAX_PREDICTED_LEAD_PX: f64 = 32.0;
    const RESYNC_AFTER_IDLE: Duration = Duration::from_millis(50);

    /// Keeps absolute Quartz events moving when WindowServer applies the
    /// previous event late. The lead is bounded so a display edge can never
    /// accumulate an arbitrarily long invisible catch-up tail.
    #[derive(Debug, Default)]
    pub(super) struct TargetTracker {
        target: Option<CGPoint>,
        last_post_at: Option<Instant>,
        display_epoch: Option<u64>,
    }

    impl TargetTracker {
        fn destination(
            &mut self,
            actual: CGPoint,
            dx: i32,
            dy: i32,
            display_epoch: u64,
            now: Instant,
        ) -> CGPoint {
            let idle = self.last_post_at.is_some_and(|last| {
                now.saturating_duration_since(last) >= RESYNC_AFTER_IDLE
            });
            let display_changed = self.display_epoch != Some(display_epoch);
            let target_too_far = self.target.is_some_and(|target| {
                (target.x - actual.x).abs() > MAX_PREDICTED_LEAD_PX
                    || (target.y - actual.y).abs() > MAX_PREDICTED_LEAD_PX
            });
            if self.target.is_none() || idle || display_changed || target_too_far {
                self.target = Some(actual);
            }

            let target = self.target.unwrap_or(actual);
            let base = CGPoint::new(
                axis_base(actual.x, target.x, dx),
                axis_base(actual.y, target.y, dy),
            );
            let requested = offset_point(base, dx, dy);
            let destination = CGPoint::new(
                requested.x.clamp(
                    actual.x - MAX_PREDICTED_LEAD_PX,
                    actual.x + MAX_PREDICTED_LEAD_PX,
                ),
                requested.y.clamp(
                    actual.y - MAX_PREDICTED_LEAD_PX,
                    actual.y + MAX_PREDICTED_LEAD_PX,
                ),
            );

            self.target = Some(destination);
            self.last_post_at = Some(now);
            self.display_epoch = Some(display_epoch);
            destination
        }
    }

    fn axis_base(actual: f64, target: f64, delta: i32) -> f64 {
        let pending = target - actual;
        let delta = f64::from(delta);
        if delta == 0.0
            || (pending > 0.0 && delta < 0.0)
            || (pending < 0.0 && delta > 0.0)
        {
            actual
        } else {
            target
        }
    }

    /// Mouse moves must remain coalescible. Under WindowServer load,
    /// non-coalesced events form a downstream queue that can keep moving the
    /// cursor after the stick has already returned to neutral.
    #[inline]
    fn movement_event_flags() -> CGEventFlags {
        CGEventFlags::empty()
    }

    /// Post a relative move while preserving Enigo's macOS semantics.
    ///
    /// Cursor position is read in native Quartz coordinates. This avoids any
    /// dependence on cached display height or AppKit's screen-layout cache,
    /// which can become stale after monitor reconfiguration.
    pub fn post(
        tracker: &mut TargetTracker,
        dx: i32,
        dy: i32,
    ) -> InputResult<MouseMoveObservation> {
        let pressed = unsafe { NSEvent::pressedMouseButtons() };

        let (event_type, button) = if pressed & 1 > 0 {
            (CGEventType::LeftMouseDragged, CGMouseButton::Left)
        } else if pressed & 2 > 0 {
            (CGEventType::RightMouseDragged, CGMouseButton::Right)
        } else {
            (CGEventType::MouseMoved, CGMouseButton::Left)
        };

        let display_epoch = super::display_configuration::epoch();
        let now = Instant::now();
        let (point, destination, event) =
            super::cg_source::with(|source| -> Result<_, ()> {
                let point = CGEvent::new(source.clone())?.location();
                let destination =
                    tracker.destination(point, dx, dy, display_epoch, now);
                let event = CGEvent::new_mouse_event(
                    source.clone(),
                    event_type,
                    destination,
                    button,
                )?;
                Ok((point, destination, event))
            })
            .map_err(|_| InputError::Simulate("failed to create mouse source"))?
            .map_err(|_| {
                InputError::Simulate("failed creating relative mouse event")
            })?;

        let (delta_x, delta_y) = movement_delta_fields(dx, dy);
        event.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_X, delta_x);
        event.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y, delta_y);
        event.set_integer_value_field(
            EventField::EVENT_SOURCE_USER_DATA,
            enigo::EVENT_MARKER as i64,
        );
        event.set_flags(movement_event_flags());
        event.post(CGEventTapLocation::HID);
        Ok(MouseMoveObservation {
            x: point.x.round() as i32,
            y: point.y.round() as i32,
            posted_dx: (destination.x - point.x).round() as i32,
            posted_dy: (destination.y - point.y).round() as i32,
            display_epoch,
        })
    }

    fn offset_point(point: CGPoint, dx: i32, dy: i32) -> CGPoint {
        CGPoint::new(point.x + f64::from(dx), point.y + f64::from(dy))
    }

    pub(super) fn movement_delta_fields(_dx: i32, _dy: i32) -> (i64, i64) {
        // The absolute Quartz destination is the source of truth. Publishing
        // the requested relative delta as well lets WindowServer accumulate
        // invisible motion after the cursor has hit a display edge.
        (0, 0)
    }

    #[cfg(test)]
    mod tests {
        use std::time::{Duration, Instant};

        use super::*;

        #[test]
        fn movement_events_allow_window_server_coalescing() {
            let flags = movement_event_flags();
            assert!(!flags.contains(CGEventFlags::CGEventFlagNonCoalesced));
            assert_eq!(flags.bits() & 0x2000_0000, 0);
        }

        #[test]
        fn quartz_coordinates_do_not_depend_on_display_height() {
            let destination =
                offset_point(CGPoint::new(757.0, 981.984_375), 12, -20);
            assert_eq!(destination.x, 769.0);
            assert_eq!(destination.y, 961.984_375);
        }

        #[test]
        fn stale_quartz_position_accumulates_only_a_bounded_lead() {
            let actual = CGPoint::new(100.0, 200.0);
            let started_at = Instant::now();
            let mut tracker = TargetTracker::default();

            let first = tracker.destination(actual, 10, 0, 0, started_at);
            let second = tracker.destination(
                actual,
                10,
                0,
                0,
                started_at + Duration::from_millis(8),
            );
            let fifth = (2..5).fold(second, |_, tick| {
                tracker.destination(
                    actual,
                    10,
                    0,
                    0,
                    started_at + Duration::from_millis(tick * 8),
                )
            });

            assert_eq!(first.x, 110.0);
            assert_eq!(second.x, 120.0);
            assert_eq!(fifth.x, 132.0);
            assert_eq!(fifth.y, actual.y);
        }

        #[test]
        fn reversing_direction_discards_unapplied_cursor_lead() {
            let actual = CGPoint::new(100.0, 200.0);
            let started_at = Instant::now();
            let mut tracker = TargetTracker::default();
            let _ = tracker.destination(actual, 20, 0, 0, started_at);
            let reversed = tracker.destination(
                actual,
                -5,
                0,
                0,
                started_at + Duration::from_millis(8),
            );

            assert_eq!(reversed.x, 95.0);
        }

        #[test]
        fn idle_or_display_change_resynchronizes_with_live_cursor() {
            let started_at = Instant::now();
            let mut tracker = TargetTracker::default();
            let _ = tracker.destination(
                CGPoint::new(100.0, 200.0),
                20,
                0,
                0,
                started_at,
            );

            let after_idle = tracker.destination(
                CGPoint::new(500.0, 600.0),
                4,
                0,
                0,
                started_at + Duration::from_millis(100),
            );
            let after_display_change = tracker.destination(
                CGPoint::new(-300.0, 50.0),
                4,
                0,
                1,
                started_at + Duration::from_millis(108),
            );

            assert_eq!(after_idle.x, 504.0);
            assert_eq!(after_idle.y, 600.0);
            assert_eq!(after_display_change.x, -296.0);
            assert_eq!(after_display_change.y, 50.0);
        }
    }
}

#[cfg(target_os = "macos")]
mod native_mouse {
    use core_graphics::{
        display::CGPoint,
        event::{
            CGEvent, CGEventTapLocation, CGEventType, CGMouseButton, EventField,
        },
    };
    use enigo::{Button, InputError, InputResult};

    struct ButtonEventSpec {
        button: CGMouseButton,
        event_type: CGEventType,
        button_number: Option<i64>,
    }

    pub(super) fn event_location(point: CGPoint) -> CGPoint {
        // Quartz already reports global coordinates with the current display
        // layout. Do not convert them through cached AppKit display geometry.
        point
    }

    fn event_spec(button: Button, pressed: bool) -> InputResult<ButtonEventSpec> {
        let (button, down, up, button_number) = match button {
            Button::Left => (
                CGMouseButton::Left,
                CGEventType::LeftMouseDown,
                CGEventType::LeftMouseUp,
                None,
            ),
            Button::Right => (
                CGMouseButton::Right,
                CGEventType::RightMouseDown,
                CGEventType::RightMouseUp,
                None,
            ),
            Button::Middle => (
                CGMouseButton::Center,
                CGEventType::OtherMouseDown,
                CGEventType::OtherMouseUp,
                Some(2),
            ),
            Button::Back => (
                CGMouseButton::Center,
                CGEventType::OtherMouseDown,
                CGEventType::OtherMouseUp,
                Some(3),
            ),
            Button::Forward => (
                CGMouseButton::Center,
                CGEventType::OtherMouseDown,
                CGEventType::OtherMouseUp,
                Some(4),
            ),
            Button::ScrollUp
            | Button::ScrollDown
            | Button::ScrollLeft
            | Button::ScrollRight => {
                return Err(InputError::InvalidInput(
                    "scroll button is not a pointer button",
                ));
            }
        };
        Ok(ButtonEventSpec {
            button,
            event_type: if pressed { down } else { up },
            button_number,
        })
    }

    fn post(button: Button, pressed: bool, click_count: i64) -> InputResult<()> {
        let spec = event_spec(button, pressed)?;
        let event = super::cg_source::with(|source| -> Result<_, ()> {
            let point = event_location(CGEvent::new(source.clone())?.location());
            CGEvent::new_mouse_event(
                source.clone(),
                spec.event_type,
                point,
                spec.button,
            )
        })
        .map_err(|_| InputError::Simulate("failed to create mouse source"))?
        .map_err(|_| InputError::Simulate("failed creating mouse button event"))?;

        if let Some(button_number) = spec.button_number {
            event.set_integer_value_field(
                EventField::MOUSE_EVENT_BUTTON_NUMBER,
                button_number,
            );
        }
        event.set_integer_value_field(
            EventField::MOUSE_EVENT_CLICK_STATE,
            click_count,
        );
        event.set_integer_value_field(
            EventField::EVENT_SOURCE_USER_DATA,
            enigo::EVENT_MARKER as i64,
        );
        event.post(CGEventTapLocation::HID);
        Ok(())
    }

    pub fn click(button: Button, count: i64) -> InputResult<()> {
        for click_count in 1..=count {
            post(button, true, click_count)?;
            post(button, false, click_count)?;
        }
        Ok(())
    }

    pub fn press(button: Button) -> InputResult<()> {
        post(button, true, 1)
    }

    pub fn release(button: Button) -> InputResult<()> {
        post(button, false, 1)
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
        CGEvent, CGEventFlags, CGEventTapLocation, EventField, ScrollEventUnit,
    };

    use enigo::{Axis, InputError, InputResult};
    use log::debug;

    #[derive(Debug, PartialEq, Eq)]
    struct ScrollEventFields {
        fixed_axis_1: i64,
        fixed_axis_2: i64,
        point_axis_1: i64,
        point_axis_2: i64,
        continuous: Option<i64>,
    }

    fn event_fields(
        horizontal: f64,
        vertical: f64,
        continuous: Option<bool>,
    ) -> ScrollEventFields {
        ScrollEventFields {
            fixed_axis_1: (vertical * 65536.0).round() as i64,
            fixed_axis_2: (horizontal * 65536.0).round() as i64,
            point_axis_1: vertical.round() as i64,
            point_axis_2: horizontal.round() as i64,
            continuous: continuous.map(i64::from),
        }
    }

    pub fn post(axis: Axis, value: f64) -> InputResult<()> {
        match axis {
            Axis::Horizontal => post_values(value, 0.0, None, false),
            Axis::Vertical => post_values(0.0, value, None, false),
        }
    }

    pub fn post_trackpad(
        horizontal: f64,
        vertical: f64,
        zoom: bool,
    ) -> InputResult<()> {
        post_values(horizontal, vertical, Some(true), zoom)
    }

    fn trackpad_event_flags(zoom: bool) -> CGEventFlags {
        if zoom {
            CGEventFlags::CGEventFlagCommand
        } else {
            CGEventFlags::empty()
        }
    }

    fn post_values(
        horizontal: f64,
        vertical: f64,
        continuous: Option<bool>,
        zoom: bool,
    ) -> InputResult<()> {
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

        let fields = event_fields(horizontal, vertical, continuous);

        debug!(
            "[smooth_scroll] horizontal={horizontal:.3} vertical={vertical:.3} continuous_override={continuous:?} fixed1={} fixed2={} point1={} point2={}",
            fields.fixed_axis_1,
            fields.fixed_axis_2,
            fields.point_axis_1,
            fields.point_axis_2
        );

        event
            .set_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1, 0);
        event
            .set_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2, 0);
        event.set_integer_value_field(
            EventField::SCROLL_WHEEL_EVENT_FIXED_POINT_DELTA_AXIS_1,
            fields.fixed_axis_1,
        );
        event.set_integer_value_field(
            EventField::SCROLL_WHEEL_EVENT_FIXED_POINT_DELTA_AXIS_2,
            fields.fixed_axis_2,
        );
        event.set_integer_value_field(
            EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1,
            fields.point_axis_1,
        );
        event.set_integer_value_field(
            EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_2,
            fields.point_axis_2,
        );
        if let Some(continuous) = fields.continuous {
            event.set_integer_value_field(
                EventField::SCROLL_WHEEL_EVENT_IS_CONTINUOUS,
                continuous,
            );
        }
        event.set_integer_value_field(
            EventField::EVENT_SOURCE_USER_DATA,
            enigo::EVENT_MARKER as i64,
        );
        event.set_flags(trackpad_event_flags(zoom));
        event.post(CGEventTapLocation::HID);
        debug!(
            "[smooth_scroll] posted horizontal={horizontal:.3} vertical={vertical:.3} continuous_override={continuous:?} zoom={zoom}"
        );
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn trackpad_fields_preserve_both_axes_and_mark_event_continuous() {
            assert_eq!(
                event_fields(1.25, -2.5, Some(true)),
                ScrollEventFields {
                    fixed_axis_1: -163_840,
                    fixed_axis_2: 81_920,
                    point_axis_1: -3,
                    point_axis_2: 1,
                    continuous: Some(1),
                }
            );
        }

        #[test]
        fn wheel_scroll_keeps_quartz_pixel_continuous_default() {
            assert_eq!(event_fields(1.0, -2.0, None).continuous, None);
        }

        #[test]
        fn zoom_trackpad_event_carries_command_flag() {
            assert!(trackpad_event_flags(true)
                .contains(CGEventFlags::CGEventFlagCommand));
            assert!(trackpad_event_flags(false).is_empty());
        }
    }
}

pub struct Performer {
    enigo: Enigo,
    #[cfg(target_os = "macos")]
    _display_observer: Option<display_configuration::Observer>,
    #[cfg(target_os = "macos")]
    mouse_target: relative_mouse::TargetTracker,
}

// SAFETY: This is safe because we're only accessing Enigo through a Mutex,
// which provides the necessary synchronization. The internal CGEventSource
// is only used on the thread that actually performs the key presses.
unsafe impl Send for Performer {}
unsafe impl Sync for Performer {}

#[cfg(all(test, target_os = "macos"))]
mod display_reconfiguration_regression_tests {
    use core_graphics::display::CGPoint;

    use super::{native_mouse, relative_mouse};

    #[test]
    fn click_uses_live_quartz_point_without_display_height_conversion() {
        let live_point = CGPoint::new(933.5, 868.7);
        let event_point = native_mouse::event_location(live_point);
        assert_eq!(event_point.x, live_point.x);
        assert_eq!(event_point.y, live_point.y);
    }

    #[test]
    fn movement_does_not_publish_hidden_relative_distance_at_screen_edges() {
        assert_eq!(relative_mouse::movement_delta_fields(20, -15), (0, 0));
    }
}

impl Performer {
    /// Create a new performer.
    pub fn new() -> Result<Self, NewConError> {
        let settings = Settings::default();
        let enigo = Enigo::new(&settings)?;
        Ok(Self {
            enigo,
            #[cfg(target_os = "macos")]
            _display_observer: display_configuration::Observer::register(),
            #[cfg(target_os = "macos")]
            mouse_target: relative_mouse::TargetTracker::default(),
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
        with_pool(|| relative_mouse::post(&mut self.mouse_target, x, y)).map(Some)
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

    /// Post one continuous, two-axis pixel scroll event, matching a trackpad
    /// gesture closely enough for browser canvases to pan diagonally.
    #[cfg(target_os = "macos")]
    pub fn trackpad_scroll(
        &mut self,
        horizontal: f64,
        vertical: f64,
        zoom: bool,
    ) -> InputResult<()> {
        with_pool(|| smooth_scroll::post_trackpad(horizontal, vertical, zoom))
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

    #[cfg(not(target_os = "macos"))]
    pub fn trackpad_scroll(
        &mut self,
        horizontal: f64,
        vertical: f64,
        _zoom: bool,
    ) -> InputResult<()> {
        self.enigo
            .scroll(horizontal.round() as i32, Axis::Horizontal)?;
        self.enigo.scroll(vertical.round() as i32, Axis::Vertical)
    }

    /// Click a mouse button.
    #[cfg(target_os = "macos")]
    pub fn mouse_click(&mut self, button: Button) -> InputResult<()> {
        with_pool(|| native_mouse::click(button, 1))
    }

    #[cfg(not(target_os = "macos"))]
    pub fn mouse_click(&mut self, button: Button) -> InputResult<()> {
        with_pool(|| self.enigo.button(button, Direction::Click))
    }

    /// Double-click a mouse button.
    #[cfg(target_os = "macos")]
    pub fn mouse_double_click(&mut self, button: Button) -> InputResult<()> {
        with_pool(|| native_mouse::click(button, 2))
    }

    #[cfg(not(target_os = "macos"))]
    pub fn mouse_double_click(&mut self, button: Button) -> InputResult<()> {
        with_pool(|| {
            self.enigo.button(button, Direction::Click)?;
            self.enigo.button(button, Direction::Click)
        })
    }

    /// Press a mouse button (hold down).
    #[cfg(target_os = "macos")]
    pub fn mouse_press(&mut self, button: Button) -> InputResult<()> {
        with_pool(|| native_mouse::press(button))
    }

    #[cfg(not(target_os = "macos"))]
    pub fn mouse_press(&mut self, button: Button) -> InputResult<()> {
        with_pool(|| self.enigo.button(button, Direction::Press))
    }

    /// Release a mouse button.
    #[cfg(target_os = "macos")]
    pub fn mouse_release(&mut self, button: Button) -> InputResult<()> {
        with_pool(|| native_mouse::release(button))
    }

    #[cfg(not(target_os = "macos"))]
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
