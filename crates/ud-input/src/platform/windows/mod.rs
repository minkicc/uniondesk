//! Windows backend.
//!
//! Capture uses two mechanisms together because neither is sufficient alone:
//!
//! * a low level mouse/keyboard hook (`WH_MOUSE_LL` / `WH_KEYBOARD_LL`) runs on
//!   our own message loop thread and can *swallow* input before the rest of the
//!   system sees it;
//! * raw input (`WM_INPUT`) is the only source of true device deltas, because a
//!   swallowed move still reports the post-acceleration cursor position through
//!   the hook, which would drift while we keep the cursor parked.
//!
//! Injection uses `SendInput` with scan codes so the receiving machine applies
//! its own keyboard layout, and with an extra info tag so our own hook ignores
//! anything we synthesise.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::OnceLock;

use tokio::sync::mpsc::UnboundedSender;
use ud_core::geom::{DisplayInfo, Point, Rect};
use ud_core::input::{InputEvent, Modifiers, MouseButton};
use windows::core::{w, BOOL};
use windows::Win32::Foundation::{
    HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEINPUT,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RID_INPUT,
    RIDEV_INPUTSINK, RIM_TYPEMOUSE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetCursorPos, GetMessageW,
    PostQuitMessage, RegisterClassW, SetCursorPos, SetTimer, SetWindowsHookExW, TranslateMessage,
    UnhookWindowsHookEx, KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT, RI_MOUSE_BUTTON_4_DOWN,
    RI_MOUSE_BUTTON_4_UP, RI_MOUSE_BUTTON_5_DOWN, RI_MOUSE_BUTTON_5_UP, RI_MOUSE_HWHEEL,
    RI_MOUSE_LEFT_BUTTON_DOWN, RI_MOUSE_LEFT_BUTTON_UP, RI_MOUSE_MIDDLE_BUTTON_DOWN,
    RI_MOUSE_MIDDLE_BUTTON_UP, RI_MOUSE_RIGHT_BUTTON_DOWN, RI_MOUSE_RIGHT_BUTTON_UP,
    RI_MOUSE_WHEEL, MONITORINFOF_PRIMARY, WHEEL_DELTA, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_INPUT,
    WM_TIMER, WNDCLASSW,
};

use super::{CaptureOptions, Command};
use crate::event::CapturedEvent;
use crate::InputError;

mod keymap;

/// Tag placed in `dwExtraInfo` so we can recognise our own synthetic events.
const INJECT_TAG: usize = 0x5544_4b31; // "UDK1"

const LLKHF_EXTENDED: u32 = 0x01;
const LLKHF_INJECTED: u32 = 0x10;
const LLKHF_UP: u32 = 0x80;
const LLMHF_INJECTED: u32 = 0x01;

const USE_KEYBOARD: u16 = 0x01;
const USE_MOUSE: u16 = 0x02;

static EVENTS: OnceLock<UnboundedSender<CapturedEvent>> = OnceLock::new();
static CAPTURE_MOUSE: AtomicBool = AtomicBool::new(false);
static CAPTURE_KEYS: AtomicBool = AtomicBool::new(false);
static PARK_X: AtomicI32 = AtomicI32::new(0);
static PARK_Y: AtomicI32 = AtomicI32::new(0);
/// `[keyboard hook, mouse hook]`, stored as integers so the static stays `Sync`.
static HOOKS: parking_lot::Mutex<[isize; 2]> = parking_lot::Mutex::new([0, 0]);

pub fn run(
    commands: Receiver<Command>,
    events: UnboundedSender<CapturedEvent>,
    ready: Sender<Result<(), InputError>>,
) {
    let _ = EVENTS.set(events);
    match unsafe { init() } {
        Ok(()) => {
            // Signal readiness *before* entering the message loop, which never
            // returns on its own. Reporting it afterwards would make every start
            // look like a timeout and silently disable input relaying.
            let _ = ready.send(Ok(()));
            unsafe { pump(commands) };
        }
        Err(err) => {
            let _ = ready.send(Err(InputError::Platform(err)));
        }
    }
}

unsafe fn init() -> Result<(), String> {
    let hinstance = GetModuleHandleW(None).map_err(|e| e.to_string())?;
    let class_name = w!("UnionDeskInputSink");
    let window_class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: hinstance.into(),
        lpszClassName: class_name,
        ..Default::default()
    };
    if RegisterClassW(&window_class) == 0 {
        // A zero return means the class already exists, which is fine.
        let err = windows::core::Error::from_win32();
        if err.code().0 as u32 != 0x8007_0582 {
            // ERROR_CLASS_ALREADY_EXISTS is not an error for our purposes; any
            // other failure means we cannot receive raw input.
            if err.code().0 as u32 != 0 {
                return Err(format!("could not register the input window class: {err}"));
            }
        }
    }

    let hwnd = CreateWindowExW(
        Default::default(),
        class_name,
        w!("UnionDesk"),
        Default::default(),
        0,
        0,
        0,
        0,
        None,
        None,
        Some(hinstance.into()),
        None,
    )
    .map_err(|e| format!("could not create the input window: {e}"))?;
    let _ = hwnd;

    // Raw input goes to our window even when it is not focused.
    let devices = [
        RAWINPUTDEVICE {
            usUsagePage: USE_KEYBOARD,
            usUsage: 0x06,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: hwnd,
        },
        RAWINPUTDEVICE {
            usUsagePage: USE_MOUSE,
            usUsage: 0x02,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: hwnd,
        },
    ];
    RegisterRawInputDevices(&devices, std::mem::size_of::<RAWINPUTDEVICE>() as u32)
        .map_err(|e| format!("could not register raw input: {e}"))?;

    // A timer whose only job is to wake the loop so queued commands run.
    SetTimer(None, 1, 10, None);
    Ok(())
}

unsafe fn pump(commands: Receiver<Command>) {
    let mut message = MSG::default();
    loop {
        let status = GetMessageW(&mut message, None, 0, 0);
        if status.0 <= 0 {
            break;
        }
        if message.message == WM_TIMER {
            if drain(&commands) {
                break;
            }
            continue;
        }
        let _ = TranslateMessage(&message);
        DispatchMessageW(&message);
    }
    uninstall_hooks();
}

/// Drains pending commands. Returns true when the thread should stop.
fn drain(commands: &Receiver<Command>) -> bool {
    while let Ok(command) = commands.try_recv() {
        match command {
            Command::SetCapture(options) => apply_capture(options),
            Command::Warp(point) => {
                let _ = warp(point);
            }
            Command::Shutdown => {
                uninstall_hooks();
                unsafe { PostQuitMessage(0) };
                return true;
            }
        }
    }
    false
}

fn apply_capture(options: CaptureOptions) {
    let want_mouse = options.mouse;
    let want_keys = options.keyboard;
    if let Some(park) = options.park_at {
        PARK_X.store(park.x.round() as i32, Ordering::Relaxed);
        PARK_Y.store(park.y.round() as i32, Ordering::Relaxed);
    }

    let installed = { HOOKS.lock()[0] != 0 };
    if want_mouse || want_keys {
        CAPTURE_MOUSE.store(want_mouse, Ordering::SeqCst);
        CAPTURE_KEYS.store(want_keys, Ordering::SeqCst);
        if !installed {
            install_hooks();
        }
    } else {
        CAPTURE_MOUSE.store(false, Ordering::SeqCst);
        CAPTURE_KEYS.store(false, Ordering::SeqCst);
        if installed {
            uninstall_hooks();
        }
    }
}

fn install_hooks() {
    let Ok(hinstance) = (unsafe { GetModuleHandleW(None) }) else {
        emit(CapturedEvent::CaptureLost {
            reason: "could not obtain the module handle".into(),
        });
        return;
    };
    let keyboard = unsafe {
        SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), Some(hinstance.into()), 0)
    };
    let mouse =
        unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), Some(hinstance.into()), 0) };
    let mut hooks = HOOKS.lock();
    match (keyboard, mouse) {
        (Ok(kb), Ok(ms)) => {
            hooks[0] = kb.0 as isize;
            hooks[1] = ms.0 as isize;
        }
        (kb, ms) => {
            if let Ok(kb) = kb {
                let _ = unsafe { UnhookWindowsHookEx(kb) };
            }
            if let Ok(ms) = ms {
                let _ = unsafe { UnhookWindowsHookEx(ms) };
            }
            CAPTURE_MOUSE.store(false, Ordering::SeqCst);
            CAPTURE_KEYS.store(false, Ordering::SeqCst);
            emit(CapturedEvent::CaptureLost {
                reason: "the operating system refused the input hooks".into(),
            });
        }
    }
}

fn uninstall_hooks() {
    let mut hooks = HOOKS.lock();
    for slot in hooks.iter_mut() {
        if *slot != 0 {
            let hook = windows::Win32::UI::WindowsAndMessaging::HHOOK(*slot as *mut core::ffi::c_void);
            let _ = unsafe { UnhookWindowsHookEx(hook) };
            *slot = 0;
        }
    }
}

fn emit(event: CapturedEvent) {
    if let Some(sender) = EVENTS.get() {
        let _ = sender.send(event);
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_INPUT {
        handle_raw_input(HRAWINPUT(lparam.0 as *mut core::ffi::c_void));
    }
    DefWindowProcW(hwnd, message, wparam, lparam)
}

unsafe fn handle_raw_input(handle: HRAWINPUT) {
    let header_size = std::mem::size_of::<windows::Win32::UI::Input::RAWINPUTHEADER>() as u32;
    let mut size = 0u32;
    let asked = GetRawInputData(handle, RID_INPUT, None, &mut size, header_size);
    if asked == u32::MAX || size == 0 {
        return;
    }
    let mut buffer = vec![0u8; size as usize];
    let written = GetRawInputData(
        handle,
        RID_INPUT,
        Some(buffer.as_mut_ptr() as *mut core::ffi::c_void),
        &mut size,
        header_size,
    );
    if written == u32::MAX || written == 0 {
        return;
    }
    let raw = &*(buffer.as_ptr() as *const RAWINPUT);
    if raw.header.dwType != RIM_TYPEMOUSE.0 {
        return;
    }
    if !CAPTURE_MOUSE.load(Ordering::Relaxed) {
        return;
    }
    let mouse = raw.data.mouse;
    let flags = mouse.Anonymous.Anonymous.usButtonFlags as u32;
    let data = mouse.Anonymous.Anonymous.usButtonData as i16 as f64;

    if flags & RI_MOUSE_LEFT_BUTTON_DOWN != 0 {
        emit(CapturedEvent::Button {
            button: MouseButton::Left,
            down: true,
        });
    }
    if flags & RI_MOUSE_LEFT_BUTTON_UP != 0 {
        emit(CapturedEvent::Button {
            button: MouseButton::Left,
            down: false,
        });
    }
    if flags & RI_MOUSE_RIGHT_BUTTON_DOWN != 0 {
        emit(CapturedEvent::Button {
            button: MouseButton::Right,
            down: true,
        });
    }
    if flags & RI_MOUSE_RIGHT_BUTTON_UP != 0 {
        emit(CapturedEvent::Button {
            button: MouseButton::Right,
            down: false,
        });
    }
    if flags & RI_MOUSE_MIDDLE_BUTTON_DOWN != 0 {
        emit(CapturedEvent::Button {
            button: MouseButton::Middle,
            down: true,
        });
    }
    if flags & RI_MOUSE_MIDDLE_BUTTON_UP != 0 {
        emit(CapturedEvent::Button {
            button: MouseButton::Middle,
            down: false,
        });
    }
    if flags & RI_MOUSE_BUTTON_4_DOWN != 0 {
        emit(CapturedEvent::Button {
            button: MouseButton::X1,
            down: true,
        });
    }
    if flags & RI_MOUSE_BUTTON_4_UP != 0 {
        emit(CapturedEvent::Button {
            button: MouseButton::X1,
            down: false,
        });
    }
    if flags & RI_MOUSE_BUTTON_5_DOWN != 0 {
        emit(CapturedEvent::Button {
            button: MouseButton::X2,
            down: true,
        });
    }
    if flags & RI_MOUSE_BUTTON_5_UP != 0 {
        emit(CapturedEvent::Button {
            button: MouseButton::X2,
            down: false,
        });
    }
    if flags & RI_MOUSE_WHEEL != 0 {
        emit(CapturedEvent::Wheel {
            dx: 0.0,
            dy: data / WHEEL_DELTA as f64,
        });
    }
    if flags & RI_MOUSE_HWHEEL != 0 {
        emit(CapturedEvent::Wheel {
            dx: data / WHEEL_DELTA as f64,
            dy: 0.0,
        });
    }

    if mouse.lLastX != 0 || mouse.lLastY != 0 {
        emit(CapturedEvent::MoveDelta {
            dx: mouse.lLastX as f64,
            dy: mouse.lLastY as f64,
        });
        // Keep the local pointer pinned where it was left.
        let _ = SetCursorPos(PARK_X.load(Ordering::Relaxed), PARK_Y.load(Ordering::Relaxed));
    }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 || !CAPTURE_MOUSE.load(Ordering::Relaxed) {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
    if info.flags & LLMHF_INJECTED != 0 || info.dwExtraInfo == INJECT_TAG {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    // Swallow every physical mouse message; raw input already reported it.
    LRESULT(1)
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 || !CAPTURE_KEYS.load(Ordering::Relaxed) {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
    if info.flags.0 & LLKHF_INJECTED != 0 || info.dwExtraInfo == INJECT_TAG {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    let mut scan = info.scanCode as u16;
    if info.flags.0 & LLKHF_EXTENDED != 0 {
        scan |= 0xe000;
    }
    let code_hid = keymap::scan_to_hid(scan).or_else(|| keymap::vk_to_hid(info.vkCode as u16));
    let down = info.flags.0 & LLKHF_UP == 0;
    match code_hid {
        Some(key) => {
            emit(CapturedEvent::Key {
                code: key,
                down,
                modifiers: modifiers(),
            });
            // Swallow so the key never reaches this machine.
            LRESULT(1)
        }
        None => {
            // Unknown keys are passed through rather than trapped; the peer
            // would not know what to press anyway.
            CallNextHookEx(None, code, wparam, lparam)
        }
    }
}

fn modifiers() -> Modifiers {
    let mut modifiers = Modifiers::empty();
    unsafe {
        let down = |vk: i32| GetAsyncKeyState(vk) as u16 & 0x8000 != 0;
        modifiers.set(Modifiers::SHIFT, down(0x10));
        modifiers.set(Modifiers::CTRL, down(0x11));
        modifiers.set(Modifiers::ALT, down(0x12));
        modifiers.set(Modifiers::META, down(0x5b) || down(0x5c));
        modifiers.set(Modifiers::CAPS_LOCK, down(0x14));
        modifiers.set(Modifiers::NUM_LOCK, down(0x90));
    }
    modifiers
}

pub fn cursor_position() -> Option<Point> {
    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point).ok()? };
    Some(Point::new(point.x as f64, point.y as f64))
}

pub fn warp(point: Point) -> Result<(), InputError> {
    unsafe { SetCursorPos(point.x.round() as i32, point.y.round() as i32) }
        .map_err(|e| InputError::Platform(e.to_string()))
}

pub fn displays() -> Vec<DisplayInfo> {
    let mut out: Vec<DisplayInfo> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(monitor_proc),
            LPARAM(&mut out as *mut Vec<DisplayInfo> as isize),
        );
    }
    if out.is_empty() {
        if let Some(point) = cursor_position() {
            out.push(DisplayInfo {
                id: "primary".into(),
                name: "Display".into(),
                bounds: Rect::new(0.0, 0.0, 1920.0, 1080.0),
                scale_factor: 1.0,
                is_primary: true,
            });
            let _ = point;
        }
    }
    out
}

unsafe extern "system" fn monitor_proc(
    monitor: HMONITOR,
    _dc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let out = &mut *(data.0 as *mut Vec<DisplayInfo>);
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(monitor, &mut info).as_bool() {
        let bounds = info.rcMonitor;
        let is_primary = info.dwFlags & MONITORINFOF_PRIMARY != 0;
        let name = format!("{}x{}", bounds.right - bounds.left, bounds.bottom - bounds.top);
        let index = out.len();
        out.push(DisplayInfo {
            id: format!("display-{index}"),
            name,
            bounds: Rect::from_corners(
                bounds.left as f64,
                bounds.top as f64,
                bounds.right as f64,
                bounds.bottom as f64,
            ),
            scale_factor: 1.0,
            is_primary,
        });
    }
    BOOL(1)
}

pub fn inject(event: &InputEvent) -> Result<(), InputError> {
    match event {
        InputEvent::MoveRel { dx, dy } => {
            let input = mouse_input(dx.round() as i32, dy.round() as i32, MOUSEEVENTF_MOVE, 0);
            send(&[input])
        }
        InputEvent::MoveAbs { x, y } => warp(Point::new(*x, *y)),
        InputEvent::Button { button, down } => {
            let flags = match (button, down) {
                (MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
                (MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
                (MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
                (MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
                (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
                (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
                (MouseButton::X1, true) => MOUSEEVENTF_XDOWN,
                (MouseButton::X1, false) => MOUSEEVENTF_XUP,
                (MouseButton::X2, true) => MOUSEEVENTF_XDOWN,
                (MouseButton::X2, false) => MOUSEEVENTF_XUP,
            };
            let data = match button {
                MouseButton::X1 => 1,
                MouseButton::X2 => 2,
                _ => 0,
            };
            send(&[mouse_input(0, 0, flags, data)])
        }
        InputEvent::Wheel { dx, dy } => {
            let mut inputs = Vec::new();
            if *dy != 0.0 {
                let data = (dy * WHEEL_DELTA as f64).round() as i32;
                inputs.push(mouse_input(0, 0, MOUSEEVENTF_WHEEL, data as u32));
            }
            if *dx != 0.0 {
                let data = (dx * WHEEL_DELTA as f64).round() as i32;
                inputs.push(mouse_input(0, 0, MOUSEEVENTF_HWHEEL, data as u32));
            }
            if inputs.is_empty() {
                Ok(())
            } else {
                send(&inputs)
            }
        }
        InputEvent::Key { code, down, .. } => {
            let Some(scan) = keymap::hid_to_scan(*code) else {
                return Err(InputError::Unsupported(format!(
                    "no scan code for HID usage {code}"
                )));
            };
            let (scan, extended) = keymap::split_scan(scan);
            let mut flags = KEYEVENTF_SCANCODE;
            if extended {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            if !down {
                flags |= KEYEVENTF_KEYUP;
            }
            let input = INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(0),
                        wScan: scan,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: INJECT_TAG,
                    },
                },
            };
            send(&[input])
        }
    }
}

fn mouse_input(dx: i32, dy: i32, flags: MOUSE_EVENT_FLAGS, data: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INJECT_TAG,
            },
        },
    }
}

fn send(inputs: &[INPUT]) -> Result<(), InputError> {
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        return Err(InputError::Platform(format!(
            "SendInput delivered {sent} of {} events",
            inputs.len()
        )));
    }
    Ok(())
}
