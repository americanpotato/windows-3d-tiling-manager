//! Geometry of the card toolbar, the move gizmo and edge snapping. Pure math: the overlay
//! does the input and the drawing.

use std::f32::consts::{PI, TAU};

use glam::{Vec2, Vec3};

/// The toolbar buttons, top to bottom.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tool {
    /// Handle to move the toolbar itself.
    Grip,
    /// Live view on/off.
    Eye,
    /// Sound in the 3D view on/off.
    Sound,
    Resize,
    /// Opens the move gizmo.
    Move,
    Snap,
    Rotate,
    /// Closes the real window (Shift: ends its process). Running windows only.
    Close,
}

impl Tool {
    /// Every button, in `Tool` order (indexes the icon and tooltip tables): a running window's card.
    pub const ALL: [Tool; 8] = [
        Tool::Grip,
        Tool::Eye,
        Tool::Sound,
        Tool::Resize,
        Tool::Move,
        Tool::Snap,
        Tool::Rotate,
        Tool::Close,
    ];
    /// Buttons of cards that aren't a running window (no live view, sound or close).
    pub const PLAIN: [Tool; 5] = [
        Tool::Grip,
        Tool::Resize,
        Tool::Move,
        Tool::Snap,
        Tool::Rotate,
    ];
}

/// Half size of a toolbar button and the gap between buttons, in metres.
pub const BTN: f32 = 0.11;
pub const GAP: f32 = 0.03;
pub const MIN_SCALE: f32 = 0.35;
pub const MAX_SCALE: f32 = 2.5;
/// How close (metres) an edge has to come to another to snap to it.
pub const SNAP: f32 = 0.35;
/// Gizmo arrow length, half thickness and tip knob half size.
pub const AXIS_LEN: f32 = 0.8;
pub const AXIS_HALF: f32 = 0.022;
pub const KNOB: f32 = 0.06;

/// A card as the geometry sees it: centre, heading and half extents.
#[derive(Clone, Copy, Debug)]
pub struct Rect3 {
    pub center: Vec3,
    pub yaw: f32,
    pub hw: f32,
    pub hh: f32,
}

pub fn right_of(yaw: f32) -> Vec3 {
    Vec3::new(yaw.cos(), 0.0, yaw.sin())
}

impl Rect3 {
    pub fn right(&self) -> Vec3 {
        right_of(self.yaw)
    }
    pub fn normal(&self) -> Vec3 {
        Vec3::new(-self.yaw.sin(), 0.0, self.yaw.cos())
    }
    pub fn left_edge(&self) -> Vec3 {
        self.center - self.right() * self.hw
    }
    pub fn right_edge(&self) -> Vec3 {
        self.center + self.right() * self.hw
    }
}

/// Angle difference in (-PI, PI].
pub fn wrap(a: f32) -> f32 {
    (a + PI).rem_euclid(TAU) - PI
}

fn flat_dist(a: Vec3, b: Vec3) -> f32 {
    Vec2::new(a.x - b.x, a.z - b.z).length()
}

/// Card-local position (x right, y up) of the top toolbar button of `me`: where the user put
/// it, else on the left, or on the right when a neighbour takes the left side.
pub fn bar_origin(me: &Rect3, others: &[Rect3], manual: Option<[f32; 2]>) -> Vec2 {
    if let Some(m) = manual {
        return Vec2::from(m);
    }
    let x = me.hw + GAP + BTN;
    let y = me.hh - BTN;
    let blocked = |side: f32| {
        let (z0, z1) = if side < 0.0 {
            (-me.hw - 2.0 * (BTN + GAP), -me.hw)
        } else {
            (me.hw, me.hw + 2.0 * (BTN + GAP))
        };
        others.iter().any(|o| {
            let d = o.center - me.center;
            let (lx, depth) = (d.dot(me.right()), d.dot(me.normal()));
            let ext = o.hw * wrap(o.yaw - me.yaw).cos().abs().max(0.2);
            depth.abs() < 0.6
                && d.y.abs() < me.hh + o.hh
                && lx + ext > z0 + 0.01
                && lx - ext < z1 - 0.01
        })
    };
    if blocked(-1.0) && !blocked(1.0) {
        Vec2::new(x, y)
    } else {
        Vec2::new(-x, y)
    }
}

/// Card-local centres of the buttons `tools`, from the top one at `origin` down.
pub fn bar_buttons(origin: Vec2, tools: &[Tool]) -> impl Iterator<Item = (Tool, Vec2)> + '_ {
    tools
        .iter()
        .enumerate()
        .map(move |(k, &t)| (t, origin - Vec2::new(0.0, k as f32 * (2.0 * BTN + GAP))))
}

/// Where `me` goes when glued to the nearest neighbour edge: (centre, yaw). Side by side the
/// cards share an edge (keeping their angle, unless nearly flat); stacked ones share a plane.
pub fn snap(me: &Rect3, others: &[Rect3]) -> Option<(Vec3, f32)> {
    let mut best: Option<(f32, Vec3, f32)> = None;
    let mut consider = |d: f32, c: Vec3, yaw: f32| {
        if best.is_none_or(|b| d < b.0) {
            best = Some((d, c, yaw));
        }
    };
    for o in others {
        let dyaw = wrap(o.yaw - me.yaw);
        if dyaw.abs() > 0.6 {
            continue;
        }
        let flat = dyaw.abs() < 0.15;
        let yaw = if flat { o.yaw } else { me.yaw };
        let r = right_of(yaw);
        // Line up centres, tops or bottoms when they are close.
        let align_y = |y: f32| {
            let top = o.center.y + o.hh - me.hh;
            let bottom = o.center.y - o.hh + me.hh;
            [o.center.y, top, bottom]
                .into_iter()
                .filter(|t| (y - t).abs() < 0.3)
                .min_by(|a, b| (y - a).abs().total_cmp(&(y - b).abs()))
                .unwrap_or(y)
        };
        if (me.center.y - o.center.y).abs() < (me.hh + o.hh) * 0.9 {
            for (mine, theirs, side) in [
                (me.left_edge(), o.right_edge(), 1.0),
                (me.right_edge(), o.left_edge(), -1.0),
            ] {
                let d = flat_dist(mine, theirs);
                if d < SNAP {
                    let mut c = theirs + r * me.hw * side;
                    c.y = align_y(me.center.y);
                    consider(d, c, yaw);
                }
            }
        }
        if flat {
            let d = me.center - o.center;
            let (lx, depth) = (d.dot(o.right()), d.dot(o.normal()));
            if lx.abs() < me.hw + o.hw && depth.abs() < SNAP {
                // Line up centres, left or right edges when they are close.
                let lx = [0.0, me.hw - o.hw, o.hw - me.hw]
                    .into_iter()
                    .filter(|t| (lx - t).abs() < 0.3)
                    .min_by(|a, b| (lx - a).abs().total_cmp(&(lx - b).abs()))
                    .unwrap_or(lx);
                for side in [1.0, -1.0] {
                    let y = o.center.y + side * (o.hh + me.hh);
                    let dist = (me.center.y - y).abs();
                    if dist < SNAP {
                        consider(
                            dist.max(depth.abs()),
                            o.center + o.right() * lx + Vec3::Y * (y - o.center.y),
                            o.yaw,
                        );
                    }
                }
            }
        }
    }
    best.map(|b| (b.1, b.2))
}

/// The edge `me` shares with a glued side neighbour, if any: (edge point, -1 left / +1 right).
pub fn shared_edge(me: &Rect3, others: &[Rect3]) -> Option<(Vec3, f32)> {
    others
        .iter()
        .filter(|o| (me.center.y - o.center.y).abs() < me.hh + o.hh)
        .find_map(|o| {
            if flat_dist(me.left_edge(), o.right_edge()) < 0.03 {
                Some((me.left_edge(), -1.0))
            } else if flat_dist(me.right_edge(), o.left_edge()) < 0.03 {
                Some((me.right_edge(), 1.0))
            } else {
                None
            }
        })
}

/// Centre of `me` with heading `yaw` and half width `hw`, keeping a shared edge in place.
pub fn keep_edge(me: &Rect3, yaw: f32, hw: f32, edge: Option<(Vec3, f32)>) -> Vec3 {
    match edge {
        Some((p, side)) => {
            let mut c = p - right_of(yaw) * hw * side;
            c.y = me.center.y;
            c
        }
        None => me.center,
    }
}

/// A rectangle in space: centre and half-extent vectors.
#[derive(Clone, Copy, Debug)]
pub struct Quad {
    pub center: Vec3,
    pub right: Vec3,
    pub up: Vec3,
}

/// Distance along the ray (`o`, `d`) to quad `q`, whose `up` extent is grown by `grow`.
pub fn ray_quad(o: Vec3, d: Vec3, q: &Quad, grow: f32) -> Option<f32> {
    let (r2, u2) = (q.right.length_squared(), q.up.length_squared());
    if r2 < 1e-10 || u2 < 1e-10 {
        return None;
    }
    let n = q.right.cross(q.up);
    let denom = n.dot(d);
    if denom.abs() < 1e-8 {
        return None;
    }
    let t = n.dot(q.center - o) / denom;
    if t <= 0.05 {
        return None;
    }
    let p = o + d * t - q.center;
    let (x, y) = (p.dot(q.right) / r2, p.dot(q.up) / u2);
    (x.abs() <= 1.0 && y.abs() <= grow).then_some(t)
}

/// The move gizmo of a card: for its X (along), Y (up) and Z (towards `eye`) axes, the arrow
/// shaft (`None` when seen end-on) and the tip knob, which always faces the camera.
pub fn gizmo(
    c: &Rect3,
    eye: Vec3,
    cam_right: Vec3,
    cam_up: Vec3,
) -> [(Vec3, Option<Quad>, Quad); 3] {
    let toward = if c.normal().dot(eye - c.center) >= 0.0 {
        c.normal()
    } else {
        -c.normal()
    };
    let origin = c.center + toward * 0.04;
    [c.right(), Vec3::Y, toward].map(|a| {
        let mid = origin + a * AXIS_LEN * 0.5;
        let side = a.cross(mid - eye);
        let shaft = (side.length() > 1e-4).then(|| Quad {
            center: mid,
            right: a * AXIS_LEN * 0.5,
            up: side.normalize() * AXIS_HALF,
        });
        let knob = Quad {
            center: origin + a * AXIS_LEN,
            right: cam_right * KNOB,
            up: cam_up * KNOB,
        };
        (a, shaft, knob)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(x: f32, y: f32, yaw: f32) -> Rect3 {
        Rect3 {
            center: Vec3::new(x, y, -4.0),
            yaw,
            hw: 1.3,
            hh: 0.8,
        }
    }

    #[test]
    fn snaps_edge_to_edge() {
        let a = card(0.0, 2.0, 0.0);
        let b = card(2.7, 2.1, 0.05);
        let (c, yaw) = snap(&b, &[a]).unwrap();
        assert!((c - Vec3::new(2.6, 2.0, -4.0)).length() < 1e-4, "{c}");
        assert_eq!(yaw, 0.0);
        assert!(
            shared_edge(
                &Rect3 {
                    center: c,
                    yaw,
                    ..b
                },
                &[a]
            )
            .is_some()
        );
        // Far away: nothing.
        assert!(snap(&card(6.0, 2.0, 0.0), &[a]).is_none());
    }

    #[test]
    fn stacks_and_rotates_about_shared_edge() {
        let a = card(0.0, 2.0, 0.0);
        let (c, _) = snap(&card(0.1, 3.7, 0.0), &[a]).unwrap();
        assert!((c - Vec3::new(0.0, 3.6, -4.0)).length() < 1e-4, "{c}");
        let b = card(2.6, 2.0, 0.0);
        let edge = shared_edge(&b, &[a]).unwrap();
        let c = keep_edge(&b, 0.4, b.hw, Some(edge));
        let turned = Rect3 {
            center: c,
            yaw: 0.4,
            ..b
        };
        assert!(flat_dist(turned.left_edge(), a.right_edge()) < 1e-4);
    }

    #[test]
    fn toolbar_moves_right_when_left_is_taken() {
        let me = card(0.0, 2.0, 0.0);
        assert!(bar_origin(&me, &[], None).x < 0.0);
        assert!(bar_origin(&me, &[card(-2.6, 2.0, 0.0)], None).x > 0.0);
        // Both sides taken: stays left (the user can drag it).
        let both = [card(-2.6, 2.0, 0.0), card(2.6, 2.0, 0.0)];
        assert!(bar_origin(&me, &both, None).x < 0.0);
    }

    #[test]
    fn ray_hits_quad() {
        let q = Quad {
            center: Vec3::new(0.0, 0.0, -2.0),
            right: Vec3::X * 0.5,
            up: Vec3::Y * 0.1,
        };
        assert!(ray_quad(Vec3::ZERO, Vec3::NEG_Z, &q, 1.0).is_some());
        assert!(ray_quad(Vec3::new(0.0, 0.2, 0.0), Vec3::NEG_Z, &q, 1.0).is_none());
        assert!(ray_quad(Vec3::new(0.0, 0.2, 0.0), Vec3::NEG_Z, &q, 3.0).is_some());
    }
}
