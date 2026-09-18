//! macOS backend.
//!
//! Capture is a `CGEventTap` placed at the HID level. Returning null from the
//! tap callback deletes an event before the rest of the system sees it, which is
//! how local input is swallowed while a peer owns the cursor. The tap is created
//! lazily and left disabled until a handover actually happens, so UnionDesk never
//! interferes with ordinary use.

use std::os::raw::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::OnceLock;

use tokio::sync::mpsc::UnboundedSender;
use ud_core::geom::{DisplayInfo, Point, Rect};
use ud_core::input::{InputEvent, Modifiers, MouseButton};

use super::{CaptureOptions, Command};
use crate::event::CapturedEvent;
use crate::InputError;

mod ffi;
mod keymap;

use ffi::*;

/// Stamped on every event we synthesise so the tap can ignore it.
const INJECT_TAG: i64 = 0x5544_4b31;

const BTN_LEFT: u8 = 1 << 0;
const BTN_RIGHT: u8 = 1 << 1;
const BTN_MIDDLE: u8 = 1 << 2;

static EVENTS: OnceLock<UnboundedSender<CapturedEvent>> = OnceLock::new();
static CAPTURE_MOUSE: AtomicBool = AtomicBool::new(false);
static CAPTURE_KEYS: AtomicBool = AtomicBool::new(false);
static WARPING: AtomicBool = AtomicBool::new(false);
static PARK_X: AtomicI32 = AtomicI32::new(0);
static PARK_Y: AtomicI32 = AtomicI32::new(0);
static BUTTONS: AtomicU8 = AtomicU8::new(0);
static TAP: AtomicUsize = AtomicUsize::new(0);
static SOURCE: AtomicUsize = AtomicUsize::new(0);

pub fn run(
    commands: Receiver<Command>,
    events: UnboundedSender<CapturedEvent>,
    ready: Sender<Result<(), InputError>>,
) {
    let _ = EVENTS.set(events);
    let _ = ready.send(Ok(()));

    loop {
        let mut stop = false;
        while let Ok(command) = commands.try_recv() {
            match command {
                Command::SetCapture(options) => apply_capture(options),
                Command::Warp(point) => {
                    let _ = warp(point);
                }
                Command::Shutdown => {
                    stop = true;
                    break;
                }
            }
        }
        if stop {
            break;
        }
        // A short slice of run loop time keeps command latency imperceptible
        // while still letting the tap deliver events promptly.
        unsafe {
            CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.008, true);
        }
    }

    let tap = TAP.swap(0, Ordering::SeqCst) as CFMachPortRef;
    if !tap.is_null() {
        unsafe {
            CGEventTapEnable(tap, false);
            CFMachPortInvalidate(tap);
            CFRelease(tap as CFTypeRef);
        }
    }
}

fn apply_capture(options: CaptureOptions) {
    if options.is_off() {
        CAPTURE_MOUSE.store(false, Ordering::SeqCst);
        CAPTURE_KEYS.store(false, Ordering::SeqCst);
        let tap = TAP.load(Ordering::SeqCst) as CFMachPortRef;
        if !tap.is_null() {
            unsafe { CGEventTapEnable(tap, false) };
        }
        return;
    }

    if let Some(park) = options.park_at {
        PARK_X.store(park.x.round() as i32, Ordering::Relaxed);
        PARK_Y.store(park.y.round() as i32, Ordering::Relaxed);
    }
    match ensure_tap() {
        Ok(tap) => {
            CAPTURE_MOUSE.store(options.mouse, Ordering::SeqCst);
            CAPTURE_KEYS.store(options.keyboard, Ordering::SeqCst);
            unsafe { CGEventTapEnable(tap, true) };
        }
        Err(err) => {
            CAPTURE_MOUSE.store(false, Ordering::SeqCst);
            CAPTURE_KEYS.store(false, Ordering::SeqCst);
            emit(CapturedEvent::CaptureLost {
                reason: err.to_string(),
            });
        }
    }
}

fn ensure_tap() -> Result<CFMachPortRef, InputError> {
    let existing = TAP.load(Ordering::SeqCst);
    if existing != 0 {
        return Ok(existing as CFMachPortRef);
    }

    let listening = unsafe { CGPreflightListenEventAccess() };
    if !listening {
        // Ask once; the answer arrives in System Settings, so the next attempt
        // after the user flips the switch will succeed.
        unsafe { CGRequestListenEventAccess() };
        return Err(InputError::Unsupported(
            "Input Monitoring permission is required. Grant it in System Settings \
             > Privacy & Security > Input Monitoring and try again."
                .into(),
        ));
    }

    let tap = unsafe {
        CGEventTapCreate(
            K_CG_HID_EVENT_TAP,
            K_CG_HEAD_INSERT_EVENT_TAP,
            K_CG_EVENT_TAP_OPTION_DEFAULT,
            event_mask(),
            tap_callback,
            std::ptr::null_mut(),
        )
    };
    if tap.is_null() {
        return Err(InputError::Unsupported(
            "the operating system refused to create an event tap. Check that \
             Accessibility and Input Monitoring are both granted."
                .into(),
        ));
    }
    unsafe {
        let source = CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0);
        if !source.is_null() {
            CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopCommonModes);
            CFRelease(source as CFTypeRef);
        }
        CGEventTapEnable(tap, false);
    }
    TAP.store(tap as usize, Ordering::SeqCst);
    Ok(tap)
}

fn event_mask() -> u64 {
    [
        K_CG_EVENT_LEFT_MOUSE_DOWN,
        K_CG_EVENT_LEFT_MOUSE_UP,
        K_CG_EVENT_RIGHT_MOUSE_DOWN,
        K_CG_EVENT_RIGHT_MOUSE_UP,
        K_CG_EVENT_MOUSE_MOVED,
        K_CG_EVENT_LEFT_MOUSE_DRAGGED,
        K_CG_EVENT_RIGHT_MOUSE_DRAGGED,
        K_CG_EVENT_OTHER_MOUSE_DRAGGED,
        K_CG_EVENT_KEY_DOWN,
        K_CG_EVENT_KEY_UP,
        K_CG_EVENT_FLAGS_CHANGED,
        K_CG_EVENT_SCROLL_WHEEL,
        K_CG_EVENT_OTHER_MOUSE_DOWN,
        K_CG_EVENT_OTHER_MOUSE_UP,
    ]
    .iter()
    .fold(0u64, |mask, event_type| mask | (1u64 << event_type))
}

fn emit(event: CapturedEvent) {
    if let Some(sender) = EVENTS.get() {
        let _ = sender.send(event);
    }
}

unsafe extern "C" fn tap_callback(
    _proxy: *mut c_void,
    event_type: u32,
    event: CGEventRef,
    _user_info: *mut c_void,
) -> CGEventRef {
    if event_type == K_CG_EVENT_TAP_DISABLED_BY_TIMEOUT
        || event_type == K_CG_EVENT_TAP_DISABLED_BY_USER_INPUT
    {
        // macOS disables a slow tap; turn it straight back on.
        let tap = TAP.load(Ordering::SeqCst) as CFMachPortRef;
        if !tap.is_null() && (CAPTURE_MOUSE.load(Ordering::Relaxed) || CAPTURE_KEYS.load(Ordering::Relaxed))
        {
            CGEventTapEnable(tap, true);
        }
        return event;
    }
    if event.is_null() {
        return event;
    }
    if WARPING.load(Ordering::Relaxed)
        || CGEventGetIntegerValueField(event, K_CG_EVENT_SOURCE_USER_DATA) == INJECT_TAG
    {
        return event;
    }

    match event_type {
        K_CG_EVENT_MOUSE_MOVED
        | K_CG_EVENT_LEFT_MOUSE_DRAGGED
        | K_CG_EVENT_RIGHT_MOUSE_DRAGGED
        | K_CG_EVENT_OTHER_MOUSE_DRAGGED => {
            if !CAPTURE_MOUSE.load(Ordering::Relaxed) {
                return event;
            }
            let dx = CGEventGetIntegerValueField(event, K_CG_MOUSE_EVENT_DELTA_X) as f64;
            let dy = CGEventGetIntegerValueField(event, K_CG_MOUSE_EVENT_DELTA_Y) as f64;
            if dx != 0.0 || dy != 0.0 {
                emit(CapturedEvent::MoveDelta { dx, dy });
            }
            park_cursor();
            std::ptr::null_mut()
        }
        K_CG_EVENT_LEFT_MOUSE_DOWN | K_CG_EVENT_RIGHT_MOUSE_DOWN | K_CG_EVENT_OTHER_MOUSE_DOWN => {
            if !CAPTURE_MOUSE.load(Ordering::Relaxed) {
                return event;
            }
            emit(CapturedEvent::Button {
                button: button_for(event_type),
                down: true,
            });
            std::ptr::null_mut()
        }
        K_CG_EVENT_LEFT_MOUSE_UP | K_CG_EVENT_RIGHT_MOUSE_UP | K_CG_EVENT_OTHER_MOUSE_UP => {
            if !CAPTURE_MOUSE.load(Ordering::Relaxed) {
                return event;
            }
            emit(CapturedEvent::Button {
                button: button_for(event_type),
                down: false,
            });
            std::ptr::null_mut()
        }
        K_CG_EVENT_SCROLL_WHEEL => {
            if !CAPTURE_MOUSE.load(Ordering::Relaxed) {
                return event;
            }
            let dy = CGEventGetIntegerValueField(event, K_CG_SCROLL_WHEEL_EVENT_DELTA_AXIS_1) as f64;
            let dx = CGEventGetIntegerValueField(event, K_CG_SCROLL_WHEEL_EVENT_DELTA_AXIS_2) as f64;
            emit(CapturedEvent::Wheel { dx, dy });
            std::ptr::null_mut()
        }
        K_CG_EVENT_KEY_DOWN | K_CG_EVENT_KEY_UP | K_CG_EVENT_FLAGS_CHANGED => {
            if !CAPTURE_KEYS.load(Ordering::Relaxed) {
                return event;
            }
            let virtual_key =
                CGEventGetIntegerValueField(event, K_CG_KEYBOARD_EVENT_KEYCODE) as u16;
            let flags = CGEventGetFlags(event);
            let down = if event_type == K_CG_EVENT_FLAGS_CHANGED {
                modifier_is_down(virtual_key, flags)
            } else {
                event_type == K_CG_EVENT_KEY_DOWN
            };
            match keymap::vk_to_hid(virtual_key) {
                Some(code) => {
                    emit(CapturedEvent::Key {
                        code,
                        down,
                        modifiers: modifiers_from(flags),
                    });
                    std::ptr::null_mut()
                }
                // Unknown keys are passed through rather than trapped.
                None => event,
            }
        }
        _ => event,
    }
}

fn park_cursor() {
    let point = CGPoint {
        x: PARK_X.load(Ordering::Relaxed) as f64,
        y: PARK_Y.load(Ordering::Relaxed) as f64,
    };
    WARPING.store(true, Ordering::SeqCst);
    unsafe { CGWarpMouseCursorPosition(point) };
    WARPING.store(false, Ordering::SeqCst);
}

fn button_for(event_type: u32) -> MouseButton {
    match event_type {
        K_CG_EVENT_LEFT_MOUSE_DOWN | K_CG_EVENT_LEFT_MOUSE_UP => MouseButton::Left,
        K_CG_EVENT_RIGHT_MOUSE_DOWN | K_CG_EVENT_RIGHT_MOUSE_UP => MouseButton::Right,
        _ => MouseButton::Middle,
    }
}

fn modifier_is_down(virtual_key: u16, flags: u64) -> bool {
    let mask = match virtual_key {
        0x38 | 0x3c => K_CG_EVENT_FLAG_MASK_SHIFT,
        0x3b | 0x3e => K_CG_EVENT_FLAG_MASK_CONTROL,
        0x3a | 0x3d => K_CG_EVENT_FLAG_MASK_ALTERNATE,
        0x37 | 0x36 => K_CG_EVENT_FLAG_MASK_COMMAND,
        0x39 => K_CG_EVENT_FLAG_MASK_ALPHA_SHIFT,
        _ => 0,
    };
    mask != 0 && flags & mask != 0
}

fn modifiers_from(flags: u64) -> Modifiers {
    let mut modifiers = Modifiers::empty();
    modifiers.set(Modifiers::SHIFT, flags & K_CG_EVENT_FLAG_MASK_SHIFT != 0);
    modifiers.set(Modifiers::CTRL, flags & K_CG_EVENT_FLAG_MASK_CONTROL != 0);
    modifiers.set(Modifiers::ALT, flags & K_CG_EVENT_FLAG_MASK_ALTERNATE != 0);
    modifiers.set(Modifiers::META, flags & K_CG_EVENT_FLAG_MASK_COMMAND != 0);
    modifiers.set(
        Modifiers::CAPS_LOCK,
        flags & K_CG_EVENT_FLAG_MASK_ALPHA_SHIFT != 0,
    );
    modifiers.set(
        Modifiers::NUM_LOCK,
        flags & K_CG_EVENT_FLAG_MASK_NUMERIC_PAD != 0,
    );
    modifiers
}

fn event_source() -> CGEventSourceRef {
    let existing = SOURCE.load(Ordering::SeqCst);
    if existing != 0 {
        return existing as CGEventSourceRef;
    }
    let source = unsafe { CGEventSourceCreate(K_CG_EVENT_SOURCE_STATE_HID_SYSTEM_STATE) };
    if !source.is_null() {
        SOURCE.store(source as usize, Ordering::SeqCst);
    }
    source
}

fn raw_cursor_position() -> CGPoint {
    unsafe {
        let event = CGEventCreate(std::ptr::null_mut());
        if event.is_null() {
            return CGPoint::default();
        }
        let point = CGEventGetLocation(event);
        CFRelease(event as CFTypeRef);
        point
    }
}

pub fn cursor_position() -> Option<Point> {
    let point = raw_cursor_position();
    Some(Point::new(point.x, point.y))
}

pub fn warp(point: Point) -> Result<(), InputError> {
    WARPING.store(true, Ordering::SeqCst);
    let status = unsafe {
        CGWarpMouseCursorPosition(CGPoint {
            x: point.x,
            y: point.y,
        })
    };
    WARPING.store(false, Ordering::SeqCst);
    if status != 0 {
        return Err(InputError::Platform(format!(
            "CGWarpMouseCursorPosition failed with status {status}"
        )));
    }
    Ok(())
}

pub fn displays() -> Vec<DisplayInfo> {
    let mut ids = [0u32; 16];
    let mut count = 0u32;
    let status = unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) };
    if status != 0 || count == 0 {
        return vec![DisplayInfo {
            id: "main".into(),
            name: "Display".into(),
            bounds: Rect::new(0.0, 0.0, 1920.0, 1080.0),
            scale_factor: 1.0,
            is_primary: true,
        }];
    }
    let main = unsafe { CGMainDisplayID() };
    (0..count.min(ids.len() as u32))
        .map(|index| {
            let id = ids[index as usize];
            let bounds = unsafe { CGDisplayBounds(id) };
            let pixels_wide = unsafe { CGDisplayPixelsWide(id) } as f64;
            let scale = if bounds.size.width > 0.0 {
                pixels_wide / bounds.size.width
            } else {
                1.0
            };
            DisplayInfo {
                id: format!("cg-{id}"),
                name: format!("{:.0}x{:.0}", bounds.size.width, bounds.size.height),
                bounds: Rect::new(
                    bounds.origin.x,
                    bounds.origin.y,
                    bounds.size.width,
                    bounds.size.height,
                ),
                scale_factor: scale,
                is_primary: id == main,
            }
        })
        .collect()
}

pub fn inject(event: &InputEvent) -> Result<(), InputError> {
    let source = event_source();
    if source.is_null() {
        return Err(InputError::Platform(
            "could not create a CoreGraphics event source".into(),
        ));
    }
    match event {
        InputEvent::MoveRel { dx, dy } => {
            let current = raw_cursor_position();
            let target = CGPoint {
                x: current.x + dx,
                y: current.y + dy,
            };
            let event_type = drag_event_type();
            unsafe {
                let created = CGEventCreateMouseEvent(source, event_type, target, drag_button());
                if created.is_null() {
                    return Err(InputError::Platform("could not create a move event".into()));
                }
                CGEventSetIntegerValueField(created, K_CG_EVENT_SOURCE_USER_DATA, INJECT_TAG);
                CGEventSetIntegerValueField(created, K_CG_MOUSE_EVENT_DELTA_X, dx.round() as i64);
                CGEventSetIntegerValueField(created, K_CG_MOUSE_EVENT_DELTA_Y, dy.round() as i64);
                CGEventPost(K_CG_HID_EVENT_TAP, created);
                CFRelease(created as CFTypeRef);
            }
            Ok(())
        }
        InputEvent::MoveAbs { x, y } => {
            let target = CGPoint { x: *x, y: *y };
            unsafe {
                let created =
                    CGEventCreateMouseEvent(source, drag_event_type(), target, drag_button());
                if created.is_null() {
                    return Err(InputError::Platform("could not create a move event".into()));
                }
                CGEventSetIntegerValueField(created, K_CG_EVENT_SOURCE_USER_DATA, INJECT_TAG);
                CGEventPost(K_CG_HID_EVENT_TAP, created);
                CFRelease(created as CFTypeRef);
            }
            Ok(())
        }
        InputEvent::Button { button, down } => {
            let (event_type, mouse_button, bit) = match (button, down) {
                (MouseButton::Left, true) => (K_CG_EVENT_LEFT_MOUSE_DOWN, K_CG_MOUSE_BUTTON_LEFT, BTN_LEFT),
                (MouseButton::Left, false) => (K_CG_EVENT_LEFT_MOUSE_UP, K_CG_MOUSE_BUTTON_LEFT, BTN_LEFT),
                (MouseButton::Right, true) => (K_CG_EVENT_RIGHT_MOUSE_DOWN, K_CG_MOUSE_BUTTON_RIGHT, BTN_RIGHT),
                (MouseButton::Right, false) => (K_CG_EVENT_RIGHT_MOUSE_UP, K_CG_MOUSE_BUTTON_RIGHT, BTN_RIGHT),
                (_, true) => (K_CG_EVENT_OTHER_MOUSE_DOWN, K_CG_MOUSE_BUTTON_CENTER, BTN_MIDDLE),
                (_, false) => (K_CG_EVENT_OTHER_MOUSE_UP, K_CG_MOUSE_BUTTON_CENTER, BTN_MIDDLE),
            };
            let position = raw_cursor_position();
            unsafe {
                let created = CGEventCreateMouseEvent(source, event_type, position, mouse_button);
                if created.is_null() {
                    return Err(InputError::Platform("could not create a button event".into()));
                }
                CGEventSetIntegerValueField(created, K_CG_EVENT_SOURCE_USER_DATA, INJECT_TAG);
                CGEventPost(K_CG_HID_EVENT_TAP, created);
                CFRelease(created as CFTypeRef);
            }
            let mask = BUTTONS.load(Ordering::SeqCst);
            BUTTONS.store(
                if *down { mask | bit } else { mask & !bit },
                Ordering::SeqCst,
            );
            Ok(())
        }
        InputEvent::Wheel { dx, dy } => unsafe {
            let created = CGEventCreateScrollWheelEvent2(
                source,
                K_CG_SCROLL_EVENT_UNIT_LINE,
                2,
                dy.round() as i32,
                dx.round() as i32,
                0,
            );
            if created.is_null() {
                return Err(InputError::Platform("could not create a scroll event".into()));
            }
            CGEventSetIntegerValueField(created, K_CG_EVENT_SOURCE_USER_DATA, INJECT_TAG);
            CGEventPost(K_CG_HID_EVENT_TAP, created);
            CFRelease(created as CFTypeRef);
            Ok(())
        },
        InputEvent::Key { code, down, .. } => {
            let Some(virtual_key) = keymap::hid_to_vk(*code) else {
                return Err(InputError::Unsupported(format!(
                    "no macOS key code for HID usage {code}"
                )));
            };
            unsafe {
                let created = CGEventCreateKeyboardEvent(source, virtual_key, *down);
                if created.is_null() {
                    return Err(InputError::Platform("could not create a key event".into()));
                }
                CGEventSetIntegerValueField(created, K_CG_EVENT_SOURCE_USER_DATA, INJECT_TAG);
                CGEventPost(K_CG_HID_EVENT_TAP, created);
                CFRelease(created as CFTypeRef);
            }
            Ok(())
        }
    }
}

fn drag_event_type() -> u32 {
    let buttons = BUTTONS.load(Ordering::SeqCst);
    if buttons & BTN_LEFT != 0 {
        K_CG_EVENT_LEFT_MOUSE_DRAGGED
    } else if buttons & BTN_RIGHT != 0 {
        K_CG_EVENT_RIGHT_MOUSE_DRAGGED
    } else if buttons & BTN_MIDDLE != 0 {
        K_CG_EVENT_OTHER_MOUSE_DRAGGED
    } else {
        K_CG_EVENT_MOUSE_MOVED
    }
}

fn drag_button() -> u32 {
    let buttons = BUTTONS.load(Ordering::SeqCst);
    if buttons & BTN_LEFT != 0 {
        K_CG_MOUSE_BUTTON_LEFT
    } else if buttons & BTN_RIGHT != 0 {
        K_CG_MOUSE_BUTTON_RIGHT
    } else {
        K_CG_MOUSE_BUTTON_CENTER
    }
}

/// Whether this process currently holds the permissions macOS requires.
pub fn permissions_granted() -> (bool, bool) {
    unsafe { (AXIsProcessTrusted(), CGPreflightListenEventAccess()) }
}
