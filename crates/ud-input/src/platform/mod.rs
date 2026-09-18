//! Per platform backend.
//!
//! Each backend owns a dedicated OS thread that runs the message or run loop
//! needed by global hooks. Everything the engine calls from other threads goes
//! through `send`, and the backend pushes observations back on an unbounded
//! channel so a slow consumer can never stall the OS input path.

use std::sync::mpsc::Sender;

use tokio::sync::mpsc::UnboundedSender;
use ud_core::geom::{DisplayInfo, Point, Rect};
use ud_core::input::InputEvent;

use crate::event::CapturedEvent;
use crate::InputError;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

// Compiles the macOS backend on other hosts purely to type-check it.
#[cfg(all(feature = "parse-check-macos", not(target_os = "macos")))]
#[path = "macos/mod.rs"]
#[allow(dead_code, unused_imports)]
mod macos_parse_check;

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
mod unsupported;

#[cfg(target_os = "macos")]
use macos as imp;
#[cfg(target_os = "windows")]
use windows as imp;
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
use unsupported as imp;

/// How the platform should treat incoming input.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaptureOptions {
    /// Swallow and report mouse movement and buttons.
    pub mouse: bool,
    /// Swallow and report key presses.
    pub keyboard: bool,
    /// Keep the local cursor pinned here so it cannot wander while a peer owns it.
    pub park_at: Option<Point>,
}

impl CaptureOptions {
    pub const OFF: CaptureOptions = CaptureOptions {
        mouse: false,
        keyboard: false,
        park_at: None,
    };

    pub fn is_off(&self) -> bool {
        !self.mouse && !self.keyboard
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Command {
    SetCapture(CaptureOptions),
    Warp(Point),
    Shutdown,
}

/// Handle to the platform thread.
pub struct Platform {
    sender: Sender<Command>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Platform {
    pub fn start(events: UnboundedSender<CapturedEvent>) -> Result<Self, InputError> {
        let (sender, receiver) = std::sync::mpsc::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("uniondesk-input".into())
            .spawn(move || imp::run(receiver, events, ready_tx))
            .map_err(|e| InputError::Platform(e.to_string()))?;

        match ready_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Platform {
                sender,
                thread: Some(thread),
            }),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(InputError::Platform(
                "the input thread did not start in time".into(),
            )),
        }
    }

    pub fn send(&self, command: Command) -> Result<(), InputError> {
        self.sender
            .send(command)
            .map_err(|_| InputError::ThreadGone)
    }

    pub fn shutdown(&mut self) {
        let _ = self.sender.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Platform {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Side effects that any thread is allowed to perform.
pub(crate) fn cursor_position() -> Option<Point> {
    imp::cursor_position()
}

pub(crate) fn displays() -> Vec<DisplayInfo> {
    imp::displays()
}

pub(crate) fn desktop_bounds() -> Rect {
    ud_core::geom::desktop_bounds(&displays())
}

pub(crate) fn inject(event: &InputEvent) -> Result<(), InputError> {
    imp::inject(event)
}
