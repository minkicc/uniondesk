//! Keyboard and mouse capture plus injection.
//!
//! # Design
//!
//! UnionDesk is passive until the cursor actually reaches a screen edge that has
//! a neighbour configured on the other side. While passive the engine simply
//! polls the cursor position, and no hooks are installed at all. That is a
//! deliberate safety property: a crash or a bug can never leave a user with a
//! keyboard that types into the void.
//!
//! When a handover does happen the backend installs its hooks, swallows local
//! input and reports raw device deltas, which the engine relays to the peer.
//! Releasing control removes the hooks again.

pub mod event;
mod platform;

pub use event::CapturedEvent;
pub use platform::CaptureOptions;

use tokio::sync::mpsc;
use ud_core::geom::{DisplayInfo, Point, Rect};
use ud_core::input::InputEvent;

use platform::{Platform, Command};

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("the input thread is no longer running")]
    ThreadGone,

    #[error("input backend failure: {0}")]
    Platform(String),

    #[error("{0}")]
    Unsupported(String),
}

pub type Result<T, E = InputError> = std::result::Result<T, E>;

/// Owns the platform input thread.
pub struct InputController {
    platform: Platform,
}

impl InputController {
    /// Starts the backend. The returned receiver carries everything the platform
    /// observed; dropping it stops event delivery but not capture.
    pub fn start() -> Result<(Self, mpsc::UnboundedReceiver<CapturedEvent>)> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let platform = Platform::start(sender)?;
        Ok((InputController { platform }, receiver))
    }

    /// Current cursor position in desktop coordinates, if the platform can say.
    pub fn cursor_position(&self) -> Option<Point> {
        platform::cursor_position()
    }

    pub fn displays(&self) -> Vec<DisplayInfo> {
        platform::displays()
    }

    pub fn desktop_bounds(&self) -> Rect {
        platform::desktop_bounds()
    }

    /// Starts or stops swallowing local input.
    pub fn set_capture(&self, options: CaptureOptions) -> Result<()> {
        self.platform.send(Command::SetCapture(options))
    }

    pub fn release_capture(&self) -> Result<()> {
        self.set_capture(CaptureOptions::OFF)
    }

    pub fn inject(&self, event: &InputEvent) -> Result<()> {
        platform::inject(event)
    }

    pub fn warp(&self, point: Point) -> Result<()> {
        self.platform.send(Command::Warp(point))
    }

    pub fn shutdown(&mut self) {
        self.platform.shutdown();
    }
}

impl Drop for InputController {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// True when this build can actually relay keyboard and mouse input.
pub const fn input_relay_supported() -> bool {
    cfg!(any(target_os = "windows", target_os = "macos"))
}

/// What the platform still needs from the user, or `None` when nothing is
/// outstanding. On macOS this is a live check, because the operating system
/// fails silently rather than reporting an error.
pub fn permission_status() -> Option<String> {
    platform::permission_status()
}

/// Kept for the about box: the message the user would see on this platform.
pub fn permission_hint() -> Option<String> {
    permission_status()
}
