//! Minimal CoreGraphics / CoreFoundation FFI.
//!
//! The safe `core-graphics` wrapper cannot express the one thing a keyboard and
//! mouse sharing tool needs most: deleting an event from the stream. Its tap
//! callback returns `Option<CGEvent>` where `None` means "keep the original",
//! so a NULL return — the documented way to swallow an event — is unreachable.
//! These declarations are therefore written by hand.

#![allow(non_camel_case_types)]

use std::os::raw::c_void;

pub type CGEventRef = *mut c_void;
pub type CGEventSourceRef = *mut c_void;
pub type CFMachPortRef = *mut c_void;
pub type CFRunLoopRef = *mut c_void;
pub type CFRunLoopSourceRef = *mut c_void;
pub type CFStringRef = *const c_void;
pub type CFAllocatorRef = *const c_void;
pub type CFTypeRef = *const c_void;
pub type CGDirectDisplayID = u32;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CGPoint {
    pub x: f64,
    pub y: f64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CGSize {
    pub width: f64,
    pub height: f64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CGRect {
    pub origin: CGPoint,
    pub size: CGSize,
}

/// Event tap callback. Returning null deletes the event.
pub type CGEventTapCallBack = unsafe extern "C" fn(
    proxy: *mut c_void,
    event_type: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef;

/// `CGEventTapLocation`
pub const K_CG_HID_EVENT_TAP: u32 = 0;
/// `CGEventTapPlacement`
pub const K_CG_HEAD_INSERT_EVENT_TAP: u32 = 0;
/// `CGEventTapOptions`
pub const K_CG_EVENT_TAP_OPTION_DEFAULT: u32 = 0;

/// `CGEventType`
pub const K_CG_EVENT_LEFT_MOUSE_DOWN: u32 = 1;
pub const K_CG_EVENT_LEFT_MOUSE_UP: u32 = 2;
pub const K_CG_EVENT_RIGHT_MOUSE_DOWN: u32 = 3;
pub const K_CG_EVENT_RIGHT_MOUSE_UP: u32 = 4;
pub const K_CG_EVENT_MOUSE_MOVED: u32 = 5;
pub const K_CG_EVENT_LEFT_MOUSE_DRAGGED: u32 = 6;
pub const K_CG_EVENT_RIGHT_MOUSE_DRAGGED: u32 = 7;
pub const K_CG_EVENT_KEY_DOWN: u32 = 10;
pub const K_CG_EVENT_KEY_UP: u32 = 11;
pub const K_CG_EVENT_FLAGS_CHANGED: u32 = 12;
pub const K_CG_EVENT_SCROLL_WHEEL: u32 = 22;
pub const K_CG_EVENT_OTHER_MOUSE_DOWN: u32 = 25;
pub const K_CG_EVENT_OTHER_MOUSE_UP: u32 = 26;
pub const K_CG_EVENT_OTHER_MOUSE_DRAGGED: u32 = 27;
pub const K_CG_EVENT_TAP_DISABLED_BY_TIMEOUT: u32 = 0xffff_fffe;
pub const K_CG_EVENT_TAP_DISABLED_BY_USER_INPUT: u32 = 0xffff_ffff;

/// `CGMouseButton`
pub const K_CG_MOUSE_BUTTON_LEFT: u32 = 0;
pub const K_CG_MOUSE_BUTTON_RIGHT: u32 = 1;
pub const K_CG_MOUSE_BUTTON_CENTER: u32 = 2;

/// `CGEventField`
pub const K_CG_MOUSE_EVENT_BUTTON_NUMBER: u32 = 3;
pub const K_CG_MOUSE_EVENT_DELTA_X: u32 = 4;
pub const K_CG_MOUSE_EVENT_DELTA_Y: u32 = 5;
pub const K_CG_KEYBOARD_EVENT_KEYCODE: u32 = 9;
pub const K_CG_SCROLL_WHEEL_EVENT_DELTA_AXIS_1: u32 = 11;
pub const K_CG_SCROLL_WHEEL_EVENT_DELTA_AXIS_2: u32 = 12;
pub const K_CG_SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1: u32 = 96;
pub const K_CG_SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_2: u32 = 97;
pub const K_CG_EVENT_SOURCE_USER_DATA: u32 = 45;

/// `CGEventFlags`
pub const K_CG_EVENT_FLAG_MASK_ALPHA_SHIFT: u64 = 0x0001_0000;
pub const K_CG_EVENT_FLAG_MASK_SHIFT: u64 = 0x0002_0000;
pub const K_CG_EVENT_FLAG_MASK_CONTROL: u64 = 0x0004_0000;
pub const K_CG_EVENT_FLAG_MASK_ALTERNATE: u64 = 0x0008_0000;
pub const K_CG_EVENT_FLAG_MASK_COMMAND: u64 = 0x0010_0000;
pub const K_CG_EVENT_FLAG_MASK_NUMERIC_PAD: u64 = 0x0020_0000;

/// `CGEventSourceStateID`
pub const K_CG_EVENT_SOURCE_STATE_HID_SYSTEM_STATE: i32 = 1;

/// `CGScrollEventUnit`
pub const K_CG_SCROLL_EVENT_UNIT_LINE: u32 = 1;

// Framework linking only makes sense on Apple targets; the `cfg_attr` keeps the
// declaration blocks usable for a type-check on other hosts.
#[cfg_attr(target_vendor = "apple", link(name = "CoreGraphics", kind = "framework"))]
#[cfg_attr(target_vendor = "apple", link(name = "CoreFoundation", kind = "framework"))]
extern "C" {
    pub fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: CGEventTapCallBack,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    pub fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);

    pub fn CGEventCreate(source: CGEventSourceRef) -> CGEventRef;
    pub fn CGEventCreateKeyboardEvent(
        source: CGEventSourceRef,
        keycode: u16,
        keydown: bool,
    ) -> CGEventRef;
    pub fn CGEventCreateMouseEvent(
        source: CGEventSourceRef,
        mouse_type: u32,
        position: CGPoint,
        button: u32,
    ) -> CGEventRef;
    pub fn CGEventCreateScrollWheelEvent2(
        source: CGEventSourceRef,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> CGEventRef;
    pub fn CGEventSourceCreate(state_id: i32) -> CGEventSourceRef;
    pub fn CGEventPost(location: u32, event: CGEventRef);
    pub fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
    pub fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    pub fn CGEventSetIntegerValueField(event: CGEventRef, field: u32, value: i64);
    pub fn CGEventGetFlags(event: CGEventRef) -> u64;
    pub fn CGEventSetFlags(event: CGEventRef, flags: u64);

    pub fn CGWarpMouseCursorPosition(point: CGPoint) -> i32;
    pub fn CGMainDisplayID() -> CGDirectDisplayID;
    pub fn CGGetActiveDisplayList(
        max_displays: u32,
        active_displays: *mut CGDirectDisplayID,
        display_count: *mut u32,
    ) -> i32;
    pub fn CGDisplayBounds(display: CGDirectDisplayID) -> CGRect;
    pub fn CGDisplayPixelsWide(display: CGDirectDisplayID) -> usize;

    pub fn CFMachPortCreateRunLoopSource(
        allocator: CFAllocatorRef,
        port: CFMachPortRef,
        order: isize,
    ) -> CFRunLoopSourceRef;
    pub fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    pub fn CFRunLoopAddSource(run_loop: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    pub fn CFRunLoopRunInMode(mode: CFStringRef, seconds: f64, return_after_source: bool) -> i32;
    pub fn CFRelease(cf: CFTypeRef);
    pub fn CFMachPortInvalidate(port: CFMachPortRef);

    pub static kCFRunLoopDefaultMode: CFStringRef;
    pub static kCFRunLoopCommonModes: CFStringRef;
}

#[cfg_attr(target_vendor = "apple", link(name = "CoreGraphics", kind = "framework"))]
extern "C" {
    /// Input Monitoring permission on macOS 10.15 and later.
    pub fn CGPreflightListenEventAccess() -> bool;
    pub fn CGRequestListenEventAccess() -> bool;
}

#[cfg_attr(target_vendor = "apple", link(name = "ApplicationServices", kind = "framework"))]
extern "C" {
    /// Accessibility permission, required to post synthetic events.
    pub fn AXIsProcessTrusted() -> bool;
}
