use ud_core::input::{KeyCode, Modifiers, MouseButton};

/// Something the platform observed on this machine.
///
/// Absolute cursor positions are polled separately; this stream only carries
/// what cannot be sampled, plus raw movement while capture is active.
#[derive(Debug, Clone, PartialEq)]
pub enum CapturedEvent {
    /// Raw device movement in mouse units, only produced while capturing.
    MoveDelta { dx: f64, dy: f64 },
    Button { button: MouseButton, down: bool },
    Wheel { dx: f64, dy: f64 },
    Key { code: KeyCode, down: bool, modifiers: Modifiers },
    /// Capture ended on its own; the engine must stop relaying input.
    CaptureLost { reason: String },
}

impl CapturedEvent {
    pub fn is_press(&self) -> bool {
        match self {
            CapturedEvent::Button { down, .. } | CapturedEvent::Key { down, .. } => *down,
            _ => false,
        }
    }
}
