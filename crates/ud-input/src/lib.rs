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

/// Asks the operating system to prompt for any permission it is still waiting
/// for. On macOS the Accessibility dialog never appears unless the application
/// explicitly asks, so without this the user is only ever told that a switch
/// somewhere is off. Call it once, from the main thread.
pub fn request_permissions() {
    platform::request_permissions()
}

/// Opens the operating system settings pane that holds a permission this
/// application is still waiting for. Does nothing where no permission is needed.
pub fn open_permission_settings() {
    platform::open_permission_settings()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ud_core::input::MouseButton;

    /// Moves the real cursor, so it is ignored by default. Run it by hand with
    /// `cargo test -p ud-input -- --ignored --nocapture`.
    ///
    /// This is the only way to check that injection actually reaches the
    /// operating system. Every other test can pass while the machine quietly
    /// refuses to move, which is exactly the failure that is hardest to notice
    /// and hardest to report.
    #[test]
    #[ignore]
    fn injection_and_warp_reach_the_operating_system() {
        let (controller, _events) = InputController::start().expect("input backend");
        let start = controller.cursor_position().expect("cursor position");
        println!("start: {start:?}");

        // Relative movement goes through the system's pointer acceleration, so
        // the cursor travels further than the raw delta. Check the direction and
        // the order of magnitude, not an exact distance.
        let relative = controller
            .inject(&InputEvent::MoveRel { dx: 40.0, dy: 25.0 })
            .map_err(|err| err.to_string());
        std::thread::sleep(std::time::Duration::from_millis(120));
        let moved = controller.cursor_position().expect("cursor position");
        println!("after relative move: {moved:?}");
        let dx = moved.x - start.x;
        let dy = moved.y - start.y;

        let absolute = controller
            .inject(&InputEvent::MoveAbs { x: 300.0, y: 260.0 })
            .map_err(|err| err.to_string());
        std::thread::sleep(std::time::Duration::from_millis(120));
        let warped = controller.cursor_position().expect("cursor position");
        println!("after absolute move: {warped:?}");

        // A button press must not leave anything held down.
        let _ = controller.inject(&InputEvent::Button {
            button: MouseButton::Left,
            down: true,
        });
        let _ = controller.inject(&InputEvent::Button {
            button: MouseButton::Left,
            down: false,
        });

        // Put the cursor back before reporting, so a failure does not leave the
        // machine somewhere unexpected.
        let _ = controller.warp(start);

        relative.expect("relative move");
        absolute.expect("absolute move");
        assert!(
            dx >= 24.0 && dy >= 15.0,
            "relative movement did not reach the cursor: moved by ({dx}, {dy})"
        );
        assert!(
            (warped.x - 300.0).abs() <= 2.0 && (warped.y - 260.0).abs() <= 2.0,
            "absolute movement did not reach the cursor: {warped:?}"
        );
    }
}
