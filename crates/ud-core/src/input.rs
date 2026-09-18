use serde::{Deserialize, Serialize};

/// Platform neutral key identity.
///
/// The value is a USB HID keyboard usage id, which every supported platform can
/// map to and from its own native key code without ambiguity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(transparent)]
pub struct KeyCode(pub u16);

impl KeyCode {
    pub const UNKNOWN: KeyCode = KeyCode(0);

    pub fn is_unknown(self) -> bool {
        self.0 == 0
    }
}

impl std::fmt::Display for KeyCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.name() {
            Some(name) => write!(f, "{name}"),
            None => write!(f, "hid-0x{:02x}", self.0),
        }
    }
}

macro_rules! define_keys {
    ($($name:ident = $value:literal),* $(,)?) => {
        /// Named HID usage ids for the default keyboard page.
        #[allow(non_upper_case_globals)]
        pub mod key {
            use super::KeyCode;
            $(pub const $name: KeyCode = KeyCode($value);)*
        }

        impl KeyCode {
            /// Reverse lookup used by the UI and by log output.
            pub fn name(self) -> Option<&'static str> {
                match self.0 {
                    $($value => Some(stringify!($name)),)*
                    _ => None,
                }
            }
        }
    };
}

define_keys! {
    A = 0x04, B = 0x05, C = 0x06, D = 0x07, E = 0x08, F = 0x09, G = 0x0a,
    H = 0x0b, I = 0x0c, J = 0x0d, K = 0x0e, L = 0x0f, M = 0x10, N = 0x11,
    O = 0x12, P = 0x13, Q = 0x14, R = 0x15, S = 0x16, T = 0x17, U = 0x18,
    V = 0x19, W = 0x1a, X = 0x1b, Y = 0x1c, Z = 0x1d,
    Num1 = 0x1e, Num2 = 0x1f, Num3 = 0x20, Num4 = 0x21, Num5 = 0x22,
    Num6 = 0x23, Num7 = 0x24, Num8 = 0x25, Num9 = 0x26, Num0 = 0x27,
    Enter = 0x28, Escape = 0x29, Backspace = 0x2a, Tab = 0x2b, Space = 0x2c,
    Minus = 0x2d, Equal = 0x2e, BracketLeft = 0x2f, BracketRight = 0x30,
    Backslash = 0x31, Semicolon = 0x33, Quote = 0x34, Backquote = 0x35,
    Comma = 0x36, Period = 0x37, Slash = 0x38, CapsLock = 0x39,
    F1 = 0x3a, F2 = 0x3b, F3 = 0x3c, F4 = 0x3d, F5 = 0x3e, F6 = 0x3f,
    F7 = 0x40, F8 = 0x41, F9 = 0x42, F10 = 0x43, F11 = 0x44, F12 = 0x45,
    PrintScreen = 0x46, ScrollLock = 0x47, Pause = 0x48, Insert = 0x49,
    Home = 0x4a, PageUp = 0x4b, Delete = 0x4c, End = 0x4d, PageDown = 0x4e,
    ArrowRight = 0x4f, ArrowLeft = 0x50, ArrowDown = 0x51, ArrowUp = 0x52,
    NumLock = 0x53, NumpadDivide = 0x54, NumpadMultiply = 0x55,
    NumpadSubtract = 0x56, NumpadAdd = 0x57, NumpadEnter = 0x58,
    Numpad1 = 0x59, Numpad2 = 0x5a, Numpad3 = 0x5b, Numpad4 = 0x5c,
    Numpad5 = 0x5d, Numpad6 = 0x5e, Numpad7 = 0x5f, Numpad8 = 0x60,
    Numpad9 = 0x61, Numpad0 = 0x62, NumpadDecimal = 0x63,
    IntlBackslash = 0x64, ContextMenu = 0x65, NumpadEqual = 0x67,
    F13 = 0x68, F14 = 0x69, F15 = 0x6a, F16 = 0x6b, F17 = 0x6c,
    F18 = 0x6d, F19 = 0x6e, F20 = 0x6f, F21 = 0x70, F22 = 0x71,
    F23 = 0x72, F24 = 0x73,
    ControlLeft = 0xe0, ShiftLeft = 0xe1, AltLeft = 0xe2, MetaLeft = 0xe3,
    ControlRight = 0xe4, ShiftRight = 0xe5, AltRight = 0xe6, MetaRight = 0xe7,
    // Media / system keys live on the consumer page but are kept here with an
    // offset so that a single u16 space can carry them over the wire.
    VolumeUp = 0x2101, VolumeDown = 0x2102, Mute = 0x2103,
    MediaPlayPause = 0x2104, MediaNext = 0x2105, MediaPrev = 0x2106,
    MediaStop = 0x2107, BrightnessUp = 0x2108, BrightnessDown = 0x2109,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Modifiers(pub u8);

impl Modifiers {
    pub const NONE: Modifiers = Modifiers(0);
    pub const SHIFT: u8 = 1 << 0;
    pub const CTRL: u8 = 1 << 1;
    pub const ALT: u8 = 1 << 2;
    pub const META: u8 = 1 << 3;
    pub const CAPS_LOCK: u8 = 1 << 4;
    pub const NUM_LOCK: u8 = 1 << 5;

    pub fn empty() -> Self {
        Modifiers(0)
    }

    pub fn contains(self, flag: u8) -> bool {
        self.0 & flag != 0
    }

    pub fn set(&mut self, flag: u8, on: bool) {
        if on {
            self.0 |= flag;
        } else {
            self.0 &= !flag;
        }
    }

    pub fn shift(self) -> bool {
        self.contains(Self::SHIFT)
    }

    pub fn ctrl(self) -> bool {
        self.contains(Self::CTRL)
    }

    pub fn alt(self) -> bool {
        self.contains(Self::ALT)
    }

    pub fn meta(self) -> bool {
        self.contains(Self::META)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

impl MouseButton {
    pub const ALL: [MouseButton; 5] = [
        MouseButton::Left,
        MouseButton::Right,
        MouseButton::Middle,
        MouseButton::X1,
        MouseButton::X2,
    ];
}

/// Everything that can travel between two machines on the input channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputEvent {
    /// Raw device movement, in mouse units. Preserved verbatim so that no
    /// acceleration curve is applied twice.
    MoveRel { dx: f64, dy: f64 },
    /// Absolute position inside the local desktop rectangle.
    MoveAbs { x: f64, y: f64 },
    Button { button: MouseButton, down: bool },
    Wheel { dx: f64, dy: f64 },
    Key {
        code: KeyCode,
        down: bool,
        modifiers: Modifiers,
    },
}

impl InputEvent {
    /// Keyboard and button events must be released before control is handed over
    /// so that the far side never sees a stuck modifier.
    pub fn is_press(&self) -> bool {
        match self {
            InputEvent::Button { down, .. } | InputEvent::Key { down, .. } => *down,
            _ => false,
        }
    }
}
