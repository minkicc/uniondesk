use serde::{Deserialize, Serialize};

/// A point in desktop pixel space.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const ZERO: Point = Point { x: 0.0, y: 0.0 };

    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// An axis aligned rectangle in desktop pixel space.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn from_corners(x0: f64, y0: f64, x1: f64, y1: f64) -> Self {
        Self::new(x0, y0, x1 - x0, y1 - y0)
    }

    pub fn left(&self) -> f64 {
        self.x
    }

    pub fn right(&self) -> f64 {
        self.x + self.width
    }

    pub fn top(&self) -> f64 {
        self.y
    }

    pub fn bottom(&self) -> f64 {
        self.y + self.height
    }

    pub fn center(&self) -> Point {
        Point::new(self.x + self.width / 2.0, self.y + self.height / 2.0)
    }

    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.left() && p.x < self.right() && p.y >= self.top() && p.y < self.bottom()
    }

    /// Smallest rectangle containing both inputs.
    pub fn union(&self, other: &Rect) -> Rect {
        let x0 = self.left().min(other.left());
        let y0 = self.top().min(other.top());
        let x1 = self.right().max(other.right());
        let y1 = self.bottom().max(other.bottom());
        Rect::from_corners(x0, y0, x1, y1)
    }

    /// Empty rectangles are ignored when folding a display list into a desktop.
    pub fn is_empty(&self) -> bool {
        self.width <= 0.0 || self.height <= 0.0
    }
}

/// A single attached display.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub id: String,
    pub name: String,
    /// Bounds in the platform's global desktop coordinate space.
    pub bounds: Rect,
    pub scale_factor: f64,
    pub is_primary: bool,
}

impl DisplayInfo {
    pub fn new(name: impl Into<String>, bounds: Rect, scale_factor: f64, is_primary: bool) -> Self {
        Self {
            id: format!(
                "{}-{:.0}x{:.0}+{:.0}+{:.0}",
                name.into(),
                bounds.width,
                bounds.height,
                bounds.x,
                bounds.y
            ),
            name: String::new(),
            bounds,
            scale_factor,
            is_primary,
        }
    }
}

/// Folds a display list into the bounding rectangle of the whole desktop.
pub fn desktop_bounds(displays: &[DisplayInfo]) -> Rect {
    let mut iter = displays.iter().filter(|d| !d.bounds.is_empty());
    let Some(first) = iter.next() else {
        return Rect::new(0.0, 0.0, 1920.0, 1080.0);
    };
    let mut acc = first.bounds;
    for display in iter {
        acc = acc.union(&display.bounds);
    }
    acc
}

/// Which edge of a rectangle a transition happens on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl Side {
    pub const ALL: [Side; 4] = [Side::Left, Side::Right, Side::Top, Side::Bottom];

    pub fn opposite(self) -> Side {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
            Side::Top => Side::Bottom,
            Side::Bottom => Side::Top,
        }
    }

    /// True when crossing this edge moves along the horizontal axis.
    pub fn is_horizontal(self) -> bool {
        matches!(self, Side::Left | Side::Right)
    }

    /// Sign of the outward direction along the movement axis.
    pub fn outward_sign(self) -> f64 {
        match self {
            Side::Right | Side::Bottom => 1.0,
            Side::Left | Side::Top => -1.0,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Side::Left => "left",
            Side::Right => "right",
            Side::Top => "top",
            Side::Bottom => "bottom",
        }
    }
}
