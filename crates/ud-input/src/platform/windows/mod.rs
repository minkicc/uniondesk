//! Windows backend.
//!
//! Capture is a pair of low level hooks (`WH_MOUSE_LL` / `WH_KEYBOARD_LL`) on a
//! dedicated thread with a message loop. Those hooks can *swallow* input before
//! the rest of the system sees it, which is what lets a peer own the keyboard
//! and mouse.
//!
//! Movement is measured from the hook's own cursor position rather than from raw
//! input. That is deliberate: raw input is not dependable while a low level hook
//! is discarding the very messages it is derived from, and a tool that silently
//! produces no deltas is worse than one that is slightly less precise.
//!
//! The trick, as used by Synergy and Barrier before it, is that the hook reports
//! the position the cursor *would* move to. The cursor is therefore pinned to a
//! fixed origin and the distance from that origin is the movement for this event.
//! The origin is the centre of the primary display, not the screen edge, because
//! positions near an edge would be clamped and the outward movement that starts a
//! handover would be lost.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::OnceLock;

use tokio::sync::mpsc::UnboundedSender;
use ud_core::geom::{DisplayInfo, Point, Rect};
use ud_core::input::{InputEvent, Modifiers, MouseButton};
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEINPUT, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL,
    MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetCursorPos, GetMessageW, GetSystemMetrics, LoadCursorW, PeekMessageW,
    PostQuitMessage, SetCursor, SetCursorPos, SetTimer, SetWindowsHookExW, TranslateMessage,
    UnhookWindowsHookEx, DispatchMessageW, KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT, MONITORINFOF_PRIMARY,
    SM_CXSCREEN, SM_CYSCREEN, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE, WM_MOUSEHWHEEL, WM_MOUSEWHEEL, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WM_TIMER, WM_XBUTTONDOWN, WM_XBUTTONUP, WHEEL_DELTA, IDC_ARROW,
    PEEK_MESSAGE_REMOVE_TYPE, PM_NOREMOVE,
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

static EVENTS: OnceLock<UnboundedSender<CapturedEvent>> = OnceLock::new();
static CAPTURE_MOUSE: AtomicBool = AtomicBool::new(false);
static CAPTURE_KEYS: AtomicBool = AtomicBool::new(false);

/// The point the cursor is pinned to while a peer owns it, and the origin that
/// movement is measured from.
static ORIGIN_X: AtomicI32 = AtomicI32::new(0);
static ORIGIN_Y: AtomicI32 = AtomicI32::new(0);

/// Observability. A handover that produces no movement should be diagnosable
/// from the log rather than guessed at.
static MOUSE_HOOK_CALLS: AtomicU64 = AtomicU64::new(0);
static KEY_HOOK_CALLS: AtomicU64 = AtomicU64::new(0);
static DELTAS_EMITTED: AtomicU64 = AtomicU64::new(0);
static FIRST_DELTA_LOGGED: AtomicBool = AtomicBool::new(false);

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

/// The low level hooks need a thread with a message queue; no window is
/// involved, which also removes a whole class of "the window never received
/// anything" problems.
unsafe fn init() -> Result<(), String> {
    let mut message = MSG::default();
    // Touching the queue first guarantees SetTimer below has somewhere to post.
    let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
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
                unsafe {
                    show_system_cursor(true);
                    PostQuitMessage(0);
                }
                return true;
            }
        }
    }
    false
}

fn apply_capture(options: CaptureOptions) {
    let want_mouse = options.mouse;
    let want_keys = options.keyboard;

    let installed = HOOKS.lock()[0] != 0;
    if !(want_mouse || want_keys) {
        CAPTURE_MOUSE.store(false, Ordering::SeqCst);
        CAPTURE_KEYS.store(false, Ordering::SeqCst);
        if installed {
            uninstall_hooks();
        }
        unsafe { show_system_cursor(true) };
        return;
    }

    if want_mouse && !CAPTURE_MOUSE.load(Ordering::SeqCst) {
        unsafe { begin_mouse_capture() };
    }
    CAPTURE_MOUSE.store(want_mouse, Ordering::SeqCst);
    CAPTURE_KEYS.store(want_keys, Ordering::SeqCst);

    if !installed {
        install_hooks();
    }
}

/// Establishes the movement origin and hides the cursor.
unsafe fn begin_mouse_capture() {
    let (x, y) = primary_centre();
    ORIGIN_X.store(x, Ordering::SeqCst);
    ORIGIN_Y.store(y, Ordering::SeqCst);
    // Move to the origin *now*, so the very next event is measured from it and
    // does not report the whole distance travelled to the screen edge.
    let _ = SetCursorPos(x, y);
    show_system_cursor(false);
    MOUSE_HOOK_CALLS.store(0, Ordering::SeqCst);
    DELTAS_EMITTED.store(0, Ordering::SeqCst);
    FIRST_DELTA_LOGGED.store(false, Ordering::SeqCst);
}

/// The centre of the primary display, which is always inside the virtual desktop
/// and far enough from every edge that a single event cannot be clamped.
unsafe fn primary_centre() -> (i32, i32) {
    let width = GetSystemMetrics(SM_CXSCREEN);
    let height = GetSystemMetrics(SM_CYSCREEN);
    (width / 2, height / 2)
}

/// Hides the cursor by giving it no shape at all. The mouse messages are being
/// swallowed, so nothing else gets a chance to set one back.
unsafe fn show_system_cursor(visible: bool) {
    if visible {
        if let Ok(arrow) = LoadCursorW(None, IDC_ARROW) {
            let _ = SetCursor(Some(arrow));
        }
    } else {
        let _ = SetCursor(None);
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
            tracing::info!("input hooks installed");
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
    if hooks[0] != 0 || hooks[1] != 0 {
        tracing::info!(
            mouse_hook_calls = MOUSE_HOOK_CALLS.load(Ordering::SeqCst),
            key_hook_calls = KEY_HOOK_CALLS.load(Ordering::SeqCst),
            deltas = DELTAS_EMITTED.load(Ordering::SeqCst),
            "releasing input capture"
        );
    }
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

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 || !CAPTURE_MOUSE.load(Ordering::Relaxed) {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
    if info.flags & LLMHF_INJECTED != 0 || info.dwExtraInfo == INJECT_TAG {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    MOUSE_HOOK_CALLS.fetch_add(1, Ordering::Relaxed);

    match wparam.0 as u32 {
        WM_MOUSEMOVE => {
            let (origin_x, origin_y) = (ORIGIN_X.load(Ordering::Relaxed), ORIGIN_Y.load(Ordering::Relaxed));
            let dx = info.pt.x - origin_x;
            let dy = info.pt.y - origin_y;
            if dx != 0 || dy != 0 {
                DELTAS_EMITTED.fetch_add(1, Ordering::Relaxed);
                if !FIRST_DELTA_LOGGED.swap(true, Ordering::Relaxed) {
                    tracing::info!(dx, dy, "first movement captured");
                }
                emit(CapturedEvent::MoveDelta {
                    dx: dx as f64,
                    dy: dy as f64,
                });
            }
            // Pin the cursor back to the origin so the next event is measured
            // from the same place, and keep it invisible while we do.
            let _ = SetCursorPos(origin_x, origin_y);
            let _ = SetCursor(None);
        }
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
        | WM_MBUTTONUP => {
            let (button, down) = match wparam.0 as u32 {
                WM_LBUTTONDOWN => (MouseButton::Left, true),
                WM_LBUTTONUP => (MouseButton::Left, false),
                WM_RBUTTONDOWN => (MouseButton::Right, true),
                WM_RBUTTONUP => (MouseButton::Right, false),
                WM_MBUTTONDOWN => (MouseButton::Middle, true),
                _ => (MouseButton::Middle, false),
            };
            emit(CapturedEvent::Button { button, down });
        }
        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let which = ((info.mouseData >> 16) & 0xffff) as u16;
            let button = if which == 2 {
                MouseButton::X2
            } else {
                MouseButton::X1
            };
            let down = wparam.0 as u32 == WM_XBUTTONDOWN;
            emit(CapturedEvent::Button { button, down });
        }
        WM_MOUSEWHEEL => {
            let delta = ((info.mouseData >> 16) & 0xffff) as u16 as i16;
            emit(CapturedEvent::Wheel {
                dx: 0.0,
                dy: delta as f64 / WHEEL_DELTA as f64,
            });
        }
        WM_MOUSEHWHEEL => {
            let delta = ((info.mouseData >> 16) & 0xffff) as u16 as i16;
            emit(CapturedEvent::Wheel {
                dx: delta as f64 / WHEEL_DELTA as f64,
                dy: 0.0,
            });
        }
        _ => {}
    }
    // Swallow every physical mouse message; the peer is the only consumer now.
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
    KEY_HOOK_CALLS.fetch_add(1, Ordering::Relaxed);

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
        let (width, height) = unsafe { primary_centre() };
        out.push(DisplayInfo {
            id: "primary".into(),
            name: "Display".into(),
            bounds: Rect::new(0.0, 0.0, (width * 2) as f64, (height * 2) as f64),
            scale_factor: 1.0,
            is_primary: true,
        });
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

#[allow(dead_code)]
fn unused(_: HWND, _: PEEK_MESSAGE_REMOVE_TYPE) {}
