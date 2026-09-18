//! Geometry of the stitched-together virtual desktop.
//!
//! Every machine reports the bounding box of its own displays. This module
//! places those boxes next to each other according to the configured
//! [`EdgeLink`]s and converts a cursor position on one machine into the matching
//! position on the other, in both directions.

use crate::config::EdgeLink;
use crate::geom::{Point, Rect, Side};
use crate::identity::DeviceId;

/// Inset used when dropping the cursor just inside the far edge so that it does
/// not immediately trigger a crossing back.
const INSET: f64 = 2.0;

/// A peer desktop placed in this machine's coordinate space.
#[derive(Debug, Clone, PartialEq)]
pub struct PlacedPeer {
    pub device_id: DeviceId,
    /// This machine's desktop rectangle, kept so the mapping is self contained.
    pub local: Rect,
    /// Bounding box of the peer's displays, in local desktop coordinates.
    pub bounds: Rect,
    /// Add this to a peer-local coordinate to obtain a local coordinate.
    pub offset: Point,
    pub link: EdgeLink,
}

impl PlacedPeer {
    pub fn to_local(&self, peer_point: Point) -> Point {
        Point::new(peer_point.x + self.offset.x, peer_point.y + self.offset.y)
    }

    pub fn to_peer(&self, local_point: Point) -> Point {
        Point::new(local_point.x - self.offset.x, local_point.y - self.offset.y)
    }
}

/// Places a peer desktop next to this one.
///
/// `peer_desktop` is the bounding box of the peer's displays *in its own
/// coordinate space*; its origin matters because a multi head peer may well
/// report a negative one, and both sides have to agree on the same mapping.
pub fn place_peer(local: Rect, peer_desktop: Rect, link: &EdgeLink) -> PlacedPeer {
    let (w, h) = (peer_desktop.width, peer_desktop.height);
    let (x, y) = match link.local_side {
        Side::Right => (
            local.right(),
            local.y + link.local_range.0 * local.height - link.remote_range.0 * h,
        ),
        Side::Left => (
            local.left() - w,
            local.y + link.local_range.0 * local.height - link.remote_range.0 * h,
        ),
        Side::Bottom => (
            local.x + link.local_range.0 * local.width - link.remote_range.0 * w,
            local.bottom(),
        ),
        Side::Top => (
            local.x + link.local_range.0 * local.width - link.remote_range.0 * w,
            local.top() - h,
        ),
    };
    PlacedPeer {
        device_id: link.peer.clone(),
        local,
        bounds: Rect::new(x, y, w, h),
        offset: Point::new(x - peer_desktop.x, y - peer_desktop.y),
        link: link.clone(),
    }
}

/// Fraction (0..1) of the cursor along the given edge of `bounds`.
pub fn fraction_along(bounds: Rect, side: Side, point: Point) -> f64 {
    let (value, start, extent) = if side.is_horizontal() {
        (point.y, bounds.y, bounds.height)
    } else {
        (point.x, bounds.x, bounds.width)
    };
    if extent.abs() < f64::EPSILON {
        return 0.5;
    }
    ((value - start) / extent).clamp(0.0, 1.0)
}

/// Re-maps a fraction from one range onto another, clamping outside the source.
pub fn map_fraction(value: f64, from: (f64, f64), to: (f64, f64)) -> f64 {
    let span = from.1 - from.0;
    if span.abs() < f64::EPSILON {
        return to.0;
    }
    let t = ((value - from.0) / span).clamp(0.0, 1.0);
    to.0 + t * (to.1 - to.0)
}

/// Point on the peer where the cursor lands, expressed in peer-local
/// coordinates (the peer's own desktop origin is subtracted back out).
pub fn entry_point(placed: &PlacedPeer, local_cursor: Point) -> Point {
    let link = &placed.link;
    let f = fraction_along(placed.local, link.local_side, local_cursor);
    let g = map_fraction(f, link.local_range, link.remote_range);
    let p = match link.remote_side {
        Side::Left => Point::new(placed.bounds.left() + INSET, placed.bounds.y + g * placed.bounds.height),
        Side::Right => Point::new(placed.bounds.right() - INSET, placed.bounds.y + g * placed.bounds.height),
        Side::Top => Point::new(placed.bounds.x + g * placed.bounds.width, placed.bounds.top() + INSET),
        Side::Bottom => Point::new(
            placed.bounds.x + g * placed.bounds.width,
            placed.bounds.bottom() - INSET,
        ),
    };
    placed.to_peer(p)
}

/// Local point where the cursor reappears when the peer hands control back.
pub fn return_point(placed: &PlacedPeer, peer_cursor: Point) -> Point {
    let link = &placed.link;
    let local = placed.local;
    let g = fraction_along(placed.bounds, link.remote_side, placed.to_local(peer_cursor));
    let f = map_fraction(g, link.remote_range, link.local_range);
    match link.local_side {
        Side::Right => Point::new(local.right() - INSET, local.y + f * local.height),
        Side::Left => Point::new(local.left() + INSET, local.y + f * local.height),
        Side::Bottom => Point::new(local.x + f * local.width, local.bottom() - INSET),
        Side::Top => Point::new(local.x + f * local.width, local.top() + INSET),
    }
}

/// True when the point lies on (or within `tolerance` of) the requested edge.
pub fn hits_edge(bounds: Rect, side: Side, point: Point, tolerance: f64) -> bool {
    let inside_y = point.y >= bounds.top() - tolerance && point.y <= bounds.bottom() + tolerance;
    let inside_x = point.x >= bounds.left() - tolerance && point.x <= bounds.right() + tolerance;
    match side {
        Side::Left => inside_y && point.x <= bounds.left() + tolerance,
        Side::Right => inside_y && point.x >= bounds.right() - tolerance,
        Side::Top => inside_x && point.y <= bounds.top() + tolerance,
        Side::Bottom => inside_x && point.y >= bounds.bottom() - tolerance,
    }
}

/// Movement has to be heading outward before a handover is allowed, otherwise a
/// cursor parked on the edge would fire continuously.
pub fn is_outward(side: Side, dx: f64, dy: f64) -> bool {
    match side {
        Side::Right => dx > 0.0,
        Side::Left => dx < 0.0,
        Side::Bottom => dy > 0.0,
        Side::Top => dy < 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EdgeLink;
    use crate::identity::DeviceId;

    fn local() -> Rect {
        Rect::new(0.0, 0.0, 1920.0, 1080.0)
    }

    fn peer_desktop() -> Rect {
        Rect::new(0.0, 0.0, 2560.0, 1440.0)
    }

    #[test]
    fn peer_is_placed_at_the_right_edge() {
        let link = EdgeLink::new(DeviceId::from("peer"), "Peer", Side::Right);
        let placed = place_peer(local(), peer_desktop(), &link);
        assert_eq!(placed.bounds.left(), 1920.0);
        assert_eq!(placed.bounds.top(), 0.0);
    }

    #[test]
    fn centred_cursor_maps_to_centred_entry() {
        let link = EdgeLink::new(DeviceId::from("peer"), "Peer", Side::Right);
        let placed = place_peer(local(), peer_desktop(), &link);
        let entry = entry_point(&placed, Point::new(1919.0, 540.0));
        assert!((entry.y - 720.0).abs() < 1.0, "entry was {entry:?}");
        // Peer local coordinates: just inside the peer's own left edge.
        assert!((entry.x - 2.0).abs() < 0.001, "entry was {entry:?}");
    }

    #[test]
    fn return_path_is_the_inverse_of_the_entry_path() {
        let link = EdgeLink::new(DeviceId::from("peer"), "Peer", Side::Right);
        let placed = place_peer(local(), peer_desktop(), &link);
        let entry = entry_point(&placed, Point::new(1919.0, 270.0));
        let back = return_point(&placed, entry);
        assert!((back.x - 1918.0).abs() < 0.001);
        assert!((back.y - 270.0).abs() < 1.0, "back was {back:?}");
    }

    #[test]
    fn a_peer_with_a_negative_origin_keeps_its_own_coordinates() {
        let link = EdgeLink::new(DeviceId::from("peer"), "Peer", Side::Right);
        let peer = Rect::new(-1920.0, -200.0, 3840.0, 1200.0);
        let placed = place_peer(local(), peer, &link);
        assert_eq!(placed.bounds.left(), 1920.0);
        let entry = entry_point(&placed, Point::new(1919.0, 540.0));
        // Round trips back to the peer's own coordinate system.
        let back = placed.to_peer(placed.to_local(entry));
        assert!((back.x - entry.x).abs() < 1e-9);
        assert!(entry.x < 0.0, "entry should live in the peer's negative space");
    }

    #[test]
    fn partial_ranges_still_map_sensibly() {
        let mut link = EdgeLink::new(DeviceId::from("peer"), "Peer", Side::Right);
        link.local_range = (0.0, 0.5);
        link.remote_range = (0.5, 1.0);
        let placed = place_peer(local(), Rect::new(0.0, 0.0, 1000.0, 1000.0), &link);
        assert_eq!(placed.bounds.top(), -500.0);
        let entry = entry_point(&placed, Point::new(1919.0, 540.0));
        // The bottom half of the local edge maps onto the bottom half of the
        // peer's edge, so the entry lands at the very bottom of that display.
        assert!((entry.y - 1000.0).abs() < 2.0, "entry was {entry:?}");
    }

    #[test]
    fn edge_detection_respects_direction() {
        let b = Rect::new(100.0, 200.0, 800.0, 600.0);
        assert!(hits_edge(b, Side::Right, Point::new(900.0, 400.0), 2.0));
        assert!(!hits_edge(b, Side::Right, Point::new(880.0, 400.0), 2.0));
        assert!(hits_edge(b, Side::Left, Point::new(100.0, 400.0), 2.0));
        assert!(is_outward(Side::Right, 3.0, 0.0));
        assert!(!is_outward(Side::Right, -3.0, 0.0));
        assert!(is_outward(Side::Top, 0.0, -1.0));
    }
}
