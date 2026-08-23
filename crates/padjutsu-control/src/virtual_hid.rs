use enigo::Button;

pub(crate) const MOUSE_REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x02, // Usage (Mouse)
    0xA1, 0x01, // Collection (Application)
    0x09, 0x01, // Usage (Pointer)
    0xA1, 0x00, // Collection (Physical)
    0x05, 0x09, // Usage Page (Button)
    0x19, 0x01, // Usage Minimum (1)
    0x29, 0x05, // Usage Maximum (5)
    0x15, 0x00, // Logical Minimum (0)
    0x25, 0x01, // Logical Maximum (1)
    0x95, 0x05, // Report Count (5)
    0x75, 0x01, // Report Size (1)
    0x81, 0x02, // Input (Data, Variable, Absolute)
    0x95, 0x01, // Report Count (1)
    0x75, 0x03, // Report Size (3)
    0x81, 0x01, // Input (Constant)
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x30, // Usage (X)
    0x09, 0x31, // Usage (Y)
    0x09, 0x38, // Usage (Wheel)
    0x15, 0x81, // Logical Minimum (-127)
    0x25, 0x7F, // Logical Maximum (127)
    0x75, 0x08, // Report Size (8)
    0x95, 0x03, // Report Count (3)
    0x81, 0x06, // Input (Data, Variable, Relative)
    0x05, 0x0C, // Usage Page (Consumer)
    0x0A, 0x38, 0x02, // Usage (AC Pan)
    0x95, 0x01, // Report Count (1)
    0x81, 0x06, // Input (Data, Variable, Relative)
    0xC0, // End Collection
    0xC0, // End Collection
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MouseReport {
    pub(crate) buttons: u8,
    pub(crate) dx: i8,
    pub(crate) dy: i8,
    pub(crate) wheel: i8,
    pub(crate) pan: i8,
}

impl MouseReport {
    pub(crate) fn bytes(self) -> [u8; 5] {
        [
            self.buttons,
            self.dx as u8,
            self.dy as u8,
            self.wheel as u8,
            self.pan as u8,
        ]
    }
}

pub(crate) trait HidReportSink {
    type Error;

    fn send_report(&mut self, report: MouseReport) -> Result<(), Self::Error>;
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum VirtualHidError<E> {
    UnsupportedButton(Button),
    Transport(E),
}

pub(crate) struct VirtualHidMouse<S> {
    sink: S,
    buttons: u8,
}

impl<S> VirtualHidMouse<S>
where
    S: HidReportSink,
{
    pub(crate) fn new(sink: S) -> Self {
        Self { sink, buttons: 0 }
    }

    pub(crate) fn move_by(
        &mut self,
        dx: i32,
        dy: i32,
    ) -> Result<(), VirtualHidError<S::Error>> {
        self.send_relative(dx, dy, 0, 0)
    }

    // Kept in the transport contract even while trackpad-style scrolling stays
    // on Quartz; a standard HID wheel backend can opt into it independently.
    #[allow(dead_code)]
    pub(crate) fn scroll(
        &mut self,
        vertical: i32,
        horizontal: i32,
    ) -> Result<(), VirtualHidError<S::Error>> {
        self.send_relative(0, 0, vertical, horizontal)
    }

    pub(crate) fn press(
        &mut self,
        button: Button,
    ) -> Result<(), VirtualHidError<S::Error>> {
        let bit = button_bit(button)?;
        let buttons = self.buttons | bit;
        if buttons == self.buttons {
            return Ok(());
        }
        self.send(MouseReport {
            buttons,
            dx: 0,
            dy: 0,
            wheel: 0,
            pan: 0,
        })?;
        self.buttons = buttons;
        Ok(())
    }

    pub(crate) fn release(
        &mut self,
        button: Button,
    ) -> Result<(), VirtualHidError<S::Error>> {
        let bit = button_bit(button)?;
        let buttons = self.buttons & !bit;
        if buttons == self.buttons {
            return Ok(());
        }
        self.send(MouseReport {
            buttons,
            dx: 0,
            dy: 0,
            wheel: 0,
            pan: 0,
        })?;
        self.buttons = buttons;
        Ok(())
    }

    pub(crate) fn click(
        &mut self,
        button: Button,
    ) -> Result<(), VirtualHidError<S::Error>> {
        self.press(button)?;
        self.release(button)
    }

    fn send_relative(
        &mut self,
        mut dx: i32,
        mut dy: i32,
        mut wheel: i32,
        mut pan: i32,
    ) -> Result<(), VirtualHidError<S::Error>> {
        while dx != 0 || dy != 0 || wheel != 0 || pan != 0 {
            let report = MouseReport {
                buttons: self.buttons,
                dx: take_axis_chunk(&mut dx),
                dy: take_axis_chunk(&mut dy),
                wheel: take_axis_chunk(&mut wheel),
                pan: take_axis_chunk(&mut pan),
            };
            self.send(report)?;
        }
        Ok(())
    }

    fn send(
        &mut self,
        report: MouseReport,
    ) -> Result<(), VirtualHidError<S::Error>> {
        self.sink
            .send_report(report)
            .map_err(VirtualHidError::Transport)
    }

    #[cfg(test)]
    fn into_sink(self) -> S {
        self.sink
    }
}

fn take_axis_chunk(remaining: &mut i32) -> i8 {
    let chunk = (*remaining).clamp(-127, 127) as i8;
    *remaining -= i32::from(chunk);
    chunk
}

fn button_bit<E>(button: Button) -> Result<u8, VirtualHidError<E>> {
    match button {
        Button::Left => Ok(1 << 0),
        Button::Right => Ok(1 << 1),
        Button::Middle => Ok(1 << 2),
        Button::Back => Ok(1 << 3),
        Button::Forward => Ok(1 << 4),
        Button::ScrollUp
        | Button::ScrollDown
        | Button::ScrollLeft
        | Button::ScrollRight => Err(VirtualHidError::UnsupportedButton(button)),
    }
}

#[cfg(target_os = "macos")]
mod native {
    use std::ffi::{c_void, CString};
    use std::fmt;
    use std::ptr;

    use super::{HidReportSink, MouseReport, MOUSE_REPORT_DESCRIPTOR};

    type CFAllocatorRef = *const c_void;
    type CFDataRef = *const c_void;
    type CFDictionaryRef = *const c_void;
    type CFIndex = isize;
    type CFMutableDictionaryRef = *mut c_void;
    type CFNumberRef = *const c_void;
    type CFStringRef = *const c_void;
    type CFTypeRef = *const c_void;
    type IOHIDUserDeviceRef = *const c_void;
    type IOOptionBits = u32;
    type IOReturn = i32;

    const K_CF_NUMBER_SINT32_TYPE: i32 = 3;
    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    const K_IORETURN_SUCCESS: IOReturn = 0;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFAllocatorDefault: CFAllocatorRef;
        static kCFTypeDictionaryKeyCallBacks: c_void;
        static kCFTypeDictionaryValueCallBacks: c_void;

        fn CFDataCreate(
            allocator: CFAllocatorRef,
            bytes: *const u8,
            length: CFIndex,
        ) -> CFDataRef;
        fn CFDictionaryCreateMutable(
            allocator: CFAllocatorRef,
            capacity: CFIndex,
            key_callbacks: *const c_void,
            value_callbacks: *const c_void,
        ) -> CFMutableDictionaryRef;
        fn CFDictionarySetValue(
            dictionary: CFMutableDictionaryRef,
            key: CFTypeRef,
            value: CFTypeRef,
        );
        fn CFNumberCreate(
            allocator: CFAllocatorRef,
            number_type: i32,
            value: *const c_void,
        ) -> CFNumberRef;
        fn CFRelease(value: CFTypeRef);
        fn CFStringCreateWithCString(
            allocator: CFAllocatorRef,
            bytes: *const i8,
            encoding: u32,
        ) -> CFStringRef;
    }

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOHIDUserDeviceCreateWithProperties(
            allocator: CFAllocatorRef,
            properties: CFDictionaryRef,
            options: IOOptionBits,
        ) -> IOHIDUserDeviceRef;
        fn IOHIDUserDeviceHandleReportWithTimeStamp(
            device: IOHIDUserDeviceRef,
            timestamp: u64,
            report: *const u8,
            report_length: CFIndex,
        ) -> IOReturn;
    }

    unsafe extern "C" {
        fn mach_absolute_time() -> u64;
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum NativeHidError {
        Allocation(&'static str),
        DeviceCreation,
        Report(IOReturn),
    }

    impl fmt::Display for NativeHidError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::Allocation(kind) => {
                    write!(formatter, "failed to allocate CoreFoundation {kind}")
                }
                Self::DeviceCreation => write!(
                    formatter,
                    "IOHIDUserDevice creation failed; the binary likely lacks com.apple.developer.hid.virtual.device"
                ),
                Self::Report(code) => {
                    write!(formatter, "IOHIDUserDevice report failed: {code:#x}")
                }
            }
        }
    }

    pub(crate) struct NativeHidSink {
        device: IOHIDUserDeviceRef,
    }

    impl NativeHidSink {
        pub(crate) fn create() -> Result<Self, NativeHidError> {
            let properties = DeviceProperties::create()?;
            let device = unsafe {
                IOHIDUserDeviceCreateWithProperties(
                    kCFAllocatorDefault,
                    properties.dictionary as CFDictionaryRef,
                    0,
                )
            };
            if device.is_null() {
                return Err(NativeHidError::DeviceCreation);
            }
            Ok(Self { device })
        }
    }

    impl HidReportSink for NativeHidSink {
        type Error = NativeHidError;

        fn send_report(&mut self, report: MouseReport) -> Result<(), Self::Error> {
            let bytes = report.bytes();
            let result = unsafe {
                IOHIDUserDeviceHandleReportWithTimeStamp(
                    self.device,
                    mach_absolute_time(),
                    bytes.as_ptr(),
                    bytes.len() as CFIndex,
                )
            };
            if result == K_IORETURN_SUCCESS {
                Ok(())
            } else {
                Err(NativeHidError::Report(result))
            }
        }
    }

    impl Drop for NativeHidSink {
        fn drop(&mut self) {
            unsafe { CFRelease(self.device) };
        }
    }

    struct DeviceProperties {
        dictionary: CFMutableDictionaryRef,
        owned_values: Vec<CFTypeRef>,
    }

    impl DeviceProperties {
        fn create() -> Result<Self, NativeHidError> {
            let dictionary = unsafe {
                CFDictionaryCreateMutable(
                    kCFAllocatorDefault,
                    7,
                    ptr::addr_of!(kCFTypeDictionaryKeyCallBacks).cast(),
                    ptr::addr_of!(kCFTypeDictionaryValueCallBacks).cast(),
                )
            };
            if dictionary.is_null() {
                return Err(NativeHidError::Allocation("dictionary"));
            }
            let mut properties = Self {
                dictionary,
                owned_values: Vec::with_capacity(14),
            };
            properties.insert_data("ReportDescriptor", MOUSE_REPORT_DESCRIPTOR)?;
            properties.insert_string("Product", "padjutsu Virtual Mouse")?;
            properties.insert_string("Manufacturer", "padjutsu")?;
            properties.insert_string("Transport", "Virtual")?;
            properties.insert_i32("VendorID", 0x504A)?;
            properties.insert_i32("ProductID", 0x0001)?;
            properties.insert_i32("PrimaryUsagePage", 0x01)?;
            properties.insert_i32("PrimaryUsage", 0x02)?;
            Ok(properties)
        }

        fn insert_data(
            &mut self,
            key: &'static str,
            value: &[u8],
        ) -> Result<(), NativeHidError> {
            let data = unsafe {
                CFDataCreate(
                    kCFAllocatorDefault,
                    value.as_ptr(),
                    value.len() as CFIndex,
                )
            };
            if data.is_null() {
                return Err(NativeHidError::Allocation("data"));
            }
            self.insert_value(key, data)
        }

        fn insert_string(
            &mut self,
            key: &'static str,
            value: &'static str,
        ) -> Result<(), NativeHidError> {
            let value = create_cf_string(value)?;
            self.insert_value(key, value)
        }

        fn insert_i32(
            &mut self,
            key: &'static str,
            value: i32,
        ) -> Result<(), NativeHidError> {
            let number = unsafe {
                CFNumberCreate(
                    kCFAllocatorDefault,
                    K_CF_NUMBER_SINT32_TYPE,
                    ptr::addr_of!(value).cast(),
                )
            };
            if number.is_null() {
                return Err(NativeHidError::Allocation("number"));
            }
            self.insert_value(key, number)
        }

        fn insert_value(
            &mut self,
            key: &'static str,
            value: CFTypeRef,
        ) -> Result<(), NativeHidError> {
            let key = create_cf_string(key)?;
            unsafe { CFDictionarySetValue(self.dictionary, key, value) };
            self.owned_values.extend([key, value]);
            Ok(())
        }
    }

    impl Drop for DeviceProperties {
        fn drop(&mut self) {
            unsafe {
                CFRelease(self.dictionary);
                for value in self.owned_values.drain(..) {
                    CFRelease(value);
                }
            }
        }
    }

    fn create_cf_string(value: &'static str) -> Result<CFStringRef, NativeHidError> {
        let value =
            CString::new(value).expect("static strings contain no NUL bytes");
        let string = unsafe {
            CFStringCreateWithCString(
                kCFAllocatorDefault,
                value.as_ptr(),
                K_CF_STRING_ENCODING_UTF8,
            )
        };
        if string.is_null() {
            Err(NativeHidError::Allocation("string"))
        } else {
            Ok(string)
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) use native::NativeHidSink;

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RecordingSink {
        reports: Vec<MouseReport>,
        fail_at: Option<usize>,
    }

    impl HidReportSink for RecordingSink {
        type Error = &'static str;

        fn send_report(&mut self, report: MouseReport) -> Result<(), Self::Error> {
            if self.fail_at == Some(self.reports.len()) {
                return Err("transport failed");
            }
            self.reports.push(report);
            Ok(())
        }
    }

    fn report(buttons: u8, dx: i8, dy: i8, wheel: i8, pan: i8) -> MouseReport {
        MouseReport {
            buttons,
            dx,
            dy,
            wheel,
            pan,
        }
    }

    #[test]
    fn descriptor_declares_five_buttons_relative_xy_wheel_and_pan() {
        assert_eq!(
            MOUSE_REPORT_DESCRIPTOR,
            &[
                0x05, 0x01, // Usage Page (Generic Desktop)
                0x09, 0x02, // Usage (Mouse)
                0xA1, 0x01, // Collection (Application)
                0x09, 0x01, // Usage (Pointer)
                0xA1, 0x00, // Collection (Physical)
                0x05, 0x09, // Usage Page (Button)
                0x19, 0x01, // Usage Minimum (1)
                0x29, 0x05, // Usage Maximum (5)
                0x15, 0x00, // Logical Minimum (0)
                0x25, 0x01, // Logical Maximum (1)
                0x95, 0x05, // Report Count (5)
                0x75, 0x01, // Report Size (1)
                0x81, 0x02, // Input (Data, Variable, Absolute)
                0x95, 0x01, // Report Count (1)
                0x75, 0x03, // Report Size (3)
                0x81, 0x01, // Input (Constant)
                0x05, 0x01, // Usage Page (Generic Desktop)
                0x09, 0x30, // Usage (X)
                0x09, 0x31, // Usage (Y)
                0x09, 0x38, // Usage (Wheel)
                0x15, 0x81, // Logical Minimum (-127)
                0x25, 0x7F, // Logical Maximum (127)
                0x75, 0x08, // Report Size (8)
                0x95, 0x03, // Report Count (3)
                0x81, 0x06, // Input (Data, Variable, Relative)
                0x05, 0x0C, // Usage Page (Consumer)
                0x0A, 0x38, 0x02, // Usage (AC Pan)
                0x95, 0x01, // Report Count (1)
                0x81, 0x06, // Input (Data, Variable, Relative)
                0xC0, // End Collection
                0xC0, // End Collection
            ]
        );
    }

    #[test]
    fn report_encoding_preserves_signed_axes() {
        assert_eq!(
            report(0b1_0101, -127, 126, -3, 4).bytes(),
            [0b1_0101, 129, 126, 253, 4]
        );
    }

    #[test]
    fn large_relative_motion_is_split_without_losing_distance() {
        let mut mouse = VirtualHidMouse::new(RecordingSink::default());

        mouse.move_by(300, -260).unwrap();

        assert_eq!(
            mouse.into_sink().reports,
            vec![
                report(0, 127, -127, 0, 0),
                report(0, 127, -127, 0, 0),
                report(0, 46, -6, 0, 0),
            ]
        );
    }

    #[test]
    fn zero_motion_does_not_emit_a_report() {
        let mut mouse = VirtualHidMouse::new(RecordingSink::default());
        mouse.move_by(0, 0).unwrap();
        assert!(mouse.into_sink().reports.is_empty());
    }

    #[test]
    fn pressed_button_state_is_carried_by_motion_and_scroll_reports() {
        let mut mouse = VirtualHidMouse::new(RecordingSink::default());

        mouse.press(Button::Left).unwrap();
        mouse.move_by(5, -7).unwrap();
        mouse.scroll(-2, 3).unwrap();
        mouse.release(Button::Left).unwrap();

        assert_eq!(
            mouse.into_sink().reports,
            vec![
                report(0b00001, 0, 0, 0, 0),
                report(0b00001, 5, -7, 0, 0),
                report(0b00001, 0, 0, -2, 3),
                report(0, 0, 0, 0, 0),
            ]
        );
    }

    #[test]
    fn all_pointer_buttons_have_stable_hid_bits() {
        let mut mouse = VirtualHidMouse::new(RecordingSink::default());
        for button in [
            Button::Left,
            Button::Right,
            Button::Middle,
            Button::Back,
            Button::Forward,
        ] {
            mouse.press(button).unwrap();
        }

        assert_eq!(
            mouse.into_sink().reports,
            vec![
                report(0b00001, 0, 0, 0, 0),
                report(0b00011, 0, 0, 0, 0),
                report(0b00111, 0, 0, 0, 0),
                report(0b01111, 0, 0, 0, 0),
                report(0b11111, 0, 0, 0, 0),
            ]
        );
    }

    #[test]
    fn click_is_an_ordered_press_release_pair() {
        let mut mouse = VirtualHidMouse::new(RecordingSink::default());
        mouse.click(Button::Right).unwrap();
        assert_eq!(
            mouse.into_sink().reports,
            vec![report(0b00010, 0, 0, 0, 0), report(0, 0, 0, 0, 0)]
        );
    }

    #[test]
    fn scroll_buttons_are_rejected_without_emitting_reports() {
        let mut mouse = VirtualHidMouse::new(RecordingSink::default());
        let result = mouse.press(Button::ScrollUp);
        assert_eq!(
            result,
            Err(VirtualHidError::UnsupportedButton(Button::ScrollUp))
        );
        assert!(mouse.into_sink().reports.is_empty());
    }

    #[test]
    fn failed_press_does_not_commit_internal_button_state() {
        let sink = RecordingSink {
            fail_at: Some(0),
            ..RecordingSink::default()
        };
        let mut mouse = VirtualHidMouse::new(sink);

        assert_eq!(
            mouse.press(Button::Left),
            Err(VirtualHidError::Transport("transport failed"))
        );
        mouse.sink.fail_at = None;
        mouse.move_by(1, 0).unwrap();

        assert_eq!(mouse.into_sink().reports, vec![report(0, 1, 0, 0, 0)]);
    }

    #[test]
    fn scroll_is_chunked_on_both_axes() {
        let mut mouse = VirtualHidMouse::new(RecordingSink::default());
        mouse.scroll(128, -129).unwrap();
        assert_eq!(
            mouse.into_sink().reports,
            vec![report(0, 0, 0, 127, -127), report(0, 0, 0, 1, -2)]
        );
    }

    #[test]
    #[ignore = "requires a binary signed with the virtual HID entitlement"]
    fn signed_native_transport_creates_device_and_accepts_report() {
        let sink = NativeHidSink::create().expect("create virtual HID mouse");
        let mut mouse = VirtualHidMouse::new(sink);
        mouse.move_by(1, 0).expect("send virtual HID report");
    }
}
