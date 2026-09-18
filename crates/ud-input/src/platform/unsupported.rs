//! Placeholder backend for platforms UnionDesk does not drive yet.
//!
//! Everything compiles and the rest of the application works; only the actual
//! keyboard and mouse relaying is unavailable.

use std::sync::mpsc::{Receiver, Sender};

use tokio::sync::mpsc::UnboundedSender;
use ud_core::geom::{DisplayInfo, Point, Rect};
use ud_core::input::InputEvent;

use super::{CaptureOptions, Command};
use crate::event::CapturedEvent;
use crate::InputError;

pub fn run(
    commands: Receiver<Command>,
    _events: UnboundedSender<CapturedEvent>,
    ready: Sender<Result<(), InputError>>,
) {
    let _ = ready.send(Ok(()));
    while let Ok(command) = commands.recv() {
        if matches!(command, Command::Shutdown) {
            break;
        }
    }
}

pub fn cursor_position() -> Option<Point> {
    None
}

pub fn displays() -> Vec<DisplayInfo> {
    vec![DisplayInfo {
        id: "primary".into(),
        name: "Display".into(),
        bounds: Rect::new(0.0, 0.0, 1920.0, 1080.0),
        scale_factor: 1.0,
        is_primary: true,
    }]
}

pub fn inject(_event: &InputEvent) -> Result<(), InputError> {
    Err(InputError::Unsupported(
        "input injection is not implemented on this platform".into(),
    ))
}

#[allow(dead_code)]
fn unused(_: CaptureOptions) {}
