//! Screen layout maths: which outer edge the cursor is on, and where to put
//! it when it arrives from another machine.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl Side {
    pub fn opposite(self) -> Side {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
            Side::Top => Side::Bottom,
            Side::Bottom => Side::Top,
        }
    }

    /// Does motion (dx, dy) push toward this side?
    pub fn pushed_by(self, dx: f64, dy: f64) -> bool {
        match self {
            Side::Left => dx < 0.0,
            Side::Right => dx > 0.0,
            Side::Top => dy < 0.0,
            Side::Bottom => dy > 0.0,
        }
    }

    fn horizontal(self) -> bool {
        matches!(self, Side::Left | Side::Right)
    }
}

/// A monitor in compositor (logical) coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    fn right(&self) -> f64 {
        self.x + self.w
    }
    fn bottom(&self) -> f64 {
        self.y + self.h
    }
    fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x - 1.0 && x < self.right() + 1.0 && y >= self.y - 1.0 && y < self.bottom() + 1.0
    }
    /// Span along the given side (y range for left/right, x range for top/bottom).
    fn span(&self, side: Side) -> (f64, f64) {
        if side.horizontal() {
            (self.y, self.bottom())
        } else {
            (self.x, self.right())
        }
    }
    /// Coordinate of the side itself (x for left/right, y for top/bottom).
    fn side_coord(&self, side: Side) -> f64 {
        match side {
            Side::Left => self.x,
            Side::Right => self.right(),
            Side::Top => self.y,
            Side::Bottom => self.bottom(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Layout {
    pub monitors: Vec<Rect>,
    /// Output names, parallel to `monitors` (may be empty in tests).
    pub names: Vec<String>,
}

const TOL: f64 = 1.5;
/// How far inside the edge a cursor arriving from another machine is placed.
const INSET: f64 = 3.0;

impl Layout {
    /// Only the monitors named in `only` (all of them if `only` is empty or
    /// matches nothing).
    pub fn sub(&self, only: &[String]) -> Layout {
        if only.is_empty() {
            return self.clone();
        }
        let (mut monitors, mut names) = (Vec::new(), Vec::new());
        for (m, n) in self.monitors.iter().zip(&self.names) {
            if only.iter().any(|o| o == n) {
                monitors.push(*m);
                names.push(n.clone());
            }
        }
        if monitors.is_empty() {
            return self.clone();
        }
        Layout { monitors, names }
    }

    /// Like `at_edge`, but only the edge of the monitors in `only` counts, and
    /// the fraction is along those monitors. Seams to monitors left out of
    /// `only` still don't count as edges.
    pub fn at_edge_on(&self, side: Side, x: f64, y: f64, only: &[String]) -> Option<f64> {
        let frac = self.at_edge(side, x, y)?;
        if only.is_empty() {
            return Some(frac);
        }
        let sub = self.sub(only);
        if !sub.monitors.iter().any(|m| m.contains(x, y)) {
            return None;
        }
        Some(sub.frac_of(side, x, y))
    }

    fn bbox(&self) -> Rect {
        let mut it = self.monitors.iter();
        let Some(first) = it.next() else {
            return Rect {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
            };
        };
        let (mut x0, mut y0, mut x1, mut y1) = (first.x, first.y, first.right(), first.bottom());
        for m in it {
            x0 = x0.min(m.x);
            y0 = y0.min(m.y);
            x1 = x1.max(m.right());
            y1 = y1.max(m.bottom());
        }
        Rect {
            x: x0,
            y: y0,
            w: (x1 - x0).max(1.0),
            h: (y1 - y0).max(1.0),
        }
    }

    /// Is another monitor directly beyond `m`'s `side` at position `along`?
    fn neighbour_beyond(&self, m: &Rect, side: Side, along: f64) -> bool {
        let edge = m.side_coord(side);
        self.monitors.iter().any(|n| {
            if n == m {
                return false;
            }
            let (a0, a1) = n.span(side);
            let touching = (n.side_coord(side.opposite()) - edge).abs() < TOL * 2.0;
            touching && along >= a0 && along < a1
        })
    }

    /// If the cursor sits on the outer boundary on `side`, return its
    /// position along that edge as a fraction (0..1) of the whole layout.
    pub fn at_edge(&self, side: Side, x: f64, y: f64) -> Option<f64> {
        let m = self.monitors.iter().find(|m| m.contains(x, y))?;
        let on_edge = match side {
            Side::Left => x <= m.x + TOL,
            Side::Right => x >= m.right() - 1.0 - TOL,
            Side::Top => y <= m.y + TOL,
            Side::Bottom => y >= m.bottom() - 1.0 - TOL,
        };
        let along = if side.horizontal() { y } else { x };
        if !on_edge || self.neighbour_beyond(m, side, along) {
            return None;
        }
        Some(self.frac_along(side, along))
    }

    /// Rough distance (px) from the cursor to the outer edge on `side`.
    pub fn distance_to(&self, side: Side, x: f64, y: f64) -> f64 {
        let b = self.bbox();
        match side {
            Side::Left => x - b.x,
            Side::Right => b.right() - 1.0 - x,
            Side::Top => y - b.y,
            Side::Bottom => b.bottom() - 1.0 - y,
        }
        .max(0.0)
    }

    /// Position of (x, y) along the given side, as a fraction of the layout.
    pub fn frac_of(&self, side: Side, x: f64, y: f64) -> f64 {
        self.frac_along(side, if side.horizontal() { y } else { x })
    }

    fn frac_along(&self, side: Side, along: f64) -> f64 {
        let b = self.bbox();
        let (a0, len) = if side.horizontal() {
            (b.y, b.h)
        } else {
            (b.x, b.w)
        };
        ((along - a0) / len).clamp(0.0, 1.0)
    }

    /// Point just inside the outer edge on `side`, at fraction `frac` along it.
    pub fn entry_point(&self, side: Side, frac: f64) -> (f64, f64) {
        let b = self.bbox();
        let (a0, len) = if side.horizontal() {
            (b.y, b.h)
        } else {
            (b.x, b.w)
        };
        let along = a0 + frac.clamp(0.0, 1.0) * len;
        // Outermost monitor on that side covering `along`; else the nearest one.
        let outer = |m: &&Rect| match side {
            Side::Left | Side::Top => -m.side_coord(side),
            Side::Right | Side::Bottom => m.side_coord(side),
        };
        let covering = self
            .monitors
            .iter()
            .filter(|m| {
                let (s0, s1) = m.span(side);
                along >= s0 && along < s1
            })
            .max_by(|a, b| outer(a).total_cmp(&outer(b)));
        let m = covering.or_else(|| {
            self.monitors.iter().min_by(|a, b| {
                let d = |m: &Rect| {
                    let (s0, s1) = m.span(side);
                    if along < s0 {
                        s0 - along
                    } else {
                        (along - s1).max(0.0)
                    }
                };
                d(a).total_cmp(&d(b))
            })
        });
        let Some(m) = m else { return (0.0, 0.0) };
        let (s0, s1) = m.span(side);
        let along = along.clamp(s0, s1 - 1.0);
        match side {
            Side::Left => (m.x + INSET, along),
            Side::Right => (m.right() - 1.0 - INSET, along),
            Side::Top => (along, m.y + INSET),
            Side::Bottom => (along, m.bottom() - 1.0 - INSET),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y, w, h }
    }

    fn two_side_by_side() -> Layout {
        Layout {
            monitors: vec![r(0.0, 0.0, 2560.0, 1440.0), r(2560.0, 0.0, 2560.0, 1440.0)],
            names: vec!["HDMI".into(), "DP".into()],
        }
    }

    #[test]
    fn edge_limited_to_one_monitor() {
        let l = two_side_by_side();
        let dp = ["DP".to_string()];
        // Bottom edge of the left monitor doesn't count…
        assert_eq!(l.at_edge_on(Side::Bottom, 1000.0, 1439.0, &dp), None);
        // …the right one's does, measured along that monitor only.
        let f = l
            .at_edge_on(Side::Bottom, 2560.0 + 640.0, 1439.0, &dp)
            .unwrap();
        assert!((f - 0.25).abs() < 1e-3);
        // Entering lands on the right monitor, rising from its bottom.
        assert_eq!(l.sub(&dp).entry_point(Side::Bottom, 0.5), (3840.0, 1436.0));
        // The seam between the two is still not an edge.
        assert_eq!(l.at_edge_on(Side::Left, 2560.0, 700.0, &dp), None);
    }

    #[test]
    fn inner_seam_is_not_an_edge() {
        let l = two_side_by_side();
        assert_eq!(l.at_edge(Side::Right, 2559.0, 700.0), None);
        assert_eq!(l.at_edge(Side::Left, 2560.0, 700.0), None);
    }

    #[test]
    fn outer_edges_are_edges() {
        let l = two_side_by_side();
        let f = l.at_edge(Side::Right, 5119.0, 720.0).unwrap();
        assert!((f - 0.5).abs() < 1e-3);
        assert!(l.at_edge(Side::Left, 0.0, 0.0).is_some());
        assert!(l.at_edge(Side::Top, 3000.0, 0.0).is_some());
        assert!(l.at_edge(Side::Right, 5000.0, 720.0).is_none());
    }

    #[test]
    fn entry_lands_inside_outer_monitor() {
        let l = two_side_by_side();
        let (x, y) = l.entry_point(Side::Right, 0.25);
        assert_eq!((x, y), (5116.0, 360.0));
        let (x, _) = l.entry_point(Side::Left, 0.25);
        assert_eq!(x, 3.0);
        // The point we land on is not itself on the edge, so no instant bounce.
        assert!(l.at_edge(Side::Right, x, y).is_none());
    }

    #[test]
    fn uneven_monitors_clamp_to_nearest() {
        // Tall monitor on the left, short one on the right.
        let l = Layout {
            monitors: vec![r(0.0, 0.0, 1000.0, 2000.0), r(1000.0, 0.0, 1000.0, 500.0)],
            names: vec![],
        };
        // Low on the right edge there's only the tall monitor's right side…
        assert!(l.at_edge(Side::Right, 999.0, 1500.0).is_some());
        // …but high up it's the short one's.
        assert!(l.at_edge(Side::Right, 999.0, 100.0).is_none());
        let (x, y) = l.entry_point(Side::Right, 0.75);
        assert_eq!((x, y), (996.0, 1500.0));
        let (x, _) = l.entry_point(Side::Right, 0.1);
        assert_eq!(x, 1996.0);
    }

    #[test]
    fn pushes() {
        assert!(Side::Left.pushed_by(-1.0, 0.0));
        assert!(!Side::Left.pushed_by(1.0, 0.0));
        assert!(Side::Bottom.pushed_by(0.0, 2.0));
    }
}
