//! The orbit camera.
//!
//! Yaw/pitch around a focus body with a **fixed world up** (ecliptic north)
//! rather than a free arcball. A tumbling arcball lets the user roll the
//! ecliptic to an arbitrary angle and never find "overhead" again; keeping the
//! ecliptic horizontal is most of what makes these views legible.

use super::math::{M4, V3, v3};
use super::scene::Bodies;
use super::state::{Focus, SolarUi};

pub const FOV_Y: f32 = std::f32::consts::FRAC_PI_4;
/// Pitch limit, just short of the pole where look-at degenerates.
pub const PITCH_LIMIT: f32 = 1.552;
/// Farthest the camera may pull back, gigametres (≈80 AU).
///
/// Enough to frame Neptune's orbit — 30 AU across — with room to spare, which
/// is the outermost thing the view draws.
pub const MAX_DIST: f32 = 12_000.0;
/// Far plane. Reversed-Z makes this ratio to the near plane a non-issue.
const FAR: f32 = 1.0e6;

pub struct Camera {
    pub view_proj: M4,
    pub eye: V3,
    pub near: f32,
    height_px: f32,
}

/// Unit vector from the focus toward the eye.
pub fn orbit_dir(yaw: f32, pitch: f32) -> V3 {
    v3(pitch.cos() * yaw.cos(), pitch.cos() * yaw.sin(), pitch.sin())
}

/// Distance limits for the current focus: never inside the body, never so far
/// that the scene degenerates to a point.
pub fn dist_range(focus_radius: f32) -> (f32, f32) {
    ((focus_radius * 1.6).max(1e-4), MAX_DIST)
}

impl Camera {
    pub fn from_view(st: &SolarUi, b: &Bodies, size_px: [f32; 2]) -> Camera {
        let v = &st.view;
        // While the tour is flying between two stations it supplies a blended
        // pivot. Without it the pivot would snap to the destination body on the
        // transition's first frame — Sun to Earth is 1 AU — and the camera would
        // teleport even though yaw, pitch and distance interpolate smoothly.
        let (focus, radius) = st.focus_override.unwrap_or_else(|| b.focus(st.focus()));
        let (lo, hi) = dist_range(radius);
        let dist = v.dist.clamp(lo, hi);
        let eye = focus + orbit_dir(v.yaw, v.pitch.clamp(-PITCH_LIMIT, PITCH_LIMIT)) * dist;

        // Track the near plane to the viewing distance: at 1 AU a fixed near
        // plane either clips the body being examined or throws away the
        // precision the close-up views need.
        let near = (dist * 0.0015).clamp(1e-5, 0.5);
        let aspect = size_px[0] / size_px[1].max(1.0);
        let proj = M4::perspective_reversed_z(FOV_Y, aspect, near, FAR);
        let view = M4::look_at(eye, focus, v3(0.0, 0.0, 1.0));

        Camera { view_proj: proj.mul(&view), eye, near, height_px: size_px[1].max(1.0) }
    }

    /// Apparent radius of a sphere, in pixels — used to give every body a
    /// minimum on-screen size regardless of the exaggeration setting.
    pub fn pixels_for(&self, pos: V3, radius: f32) -> f32 {
        let d = (pos - self.eye).len().max(1e-9);
        (radius / d) / (FOV_Y * 0.5).tan() * (self.height_px * 0.5)
    }
}

// ── The AUTO tour ───────────────────────────────────────────────────────────

/// One framed viewpoint in the tour.
///
/// Orientations are given *relative to a live direction* wherever the
/// composition depends on one (the Sun's bearing from the Earth, say), so a
/// station holds its framing as the bodies move rather than drifting out of it
/// over the minutes the loop takes.
pub struct Station {
    pub name: &'static str,
    pub focus: Focus,
    /// Yaw relative to `relative_to`, radians.
    pub yaw_offset: f32,
    pub pitch: f32,
    /// What the yaw is measured from.
    pub relative_to: Bearing,
    /// Distance, as a multiple of the focus body's radius.
    pub radii: f32,
    pub dwell_s: f32,
    /// Slow yaw drift while holding the station, radians/second. Keeps a long
    /// dwell from reading as a frozen frame.
    pub drift: f32,
}

/// The live direction a station's yaw is measured against.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Bearing {
    /// Absolute yaw in the ecliptic frame.
    World,
    /// The Sun's bearing as seen from the Earth.
    SunFromEarth,
    /// The Earth's bearing as seen from the Sun.
    EarthFromSun,
    /// The Moon's bearing as seen from the Earth.
    MoonFromEarth,
}

const DEG: f32 = std::f32::consts::PI / 180.0;

/// The tour, in order. Between them the camera eases for [`TRANSITION_S`].
pub const STATIONS: &[Station] = &[
    Station {
        name: "ECLIPTIC OVERHEAD",
        focus: Focus::Sun,
        yaw_offset: 0.0,
        pitch: 84.0 * DEG,
        relative_to: Bearing::World,
        radii: 500.0,
        dwell_s: 14.0,
        drift: 1.8 * DEG,
    },
    Station {
        name: "EARTH SHOULDER",
        focus: Focus::Earth,
        // Behind the Earth, looking past it at the Sun.
        yaw_offset: 180.0 * DEG,
        pitch: 22.0 * DEG,
        relative_to: Bearing::SunFromEarth,
        radii: 14.0,
        dwell_s: 12.0,
        drift: 0.9 * DEG,
    },
    Station {
        name: "SUNSIDE",
        focus: Focus::Sun,
        // Face-on to the disk SDO photographs, from the Earth's direction.
        yaw_offset: 0.0,
        pitch: 6.0 * DEG,
        relative_to: Bearing::EarthFromSun,
        radii: 3.4,
        dwell_s: 16.0,
        drift: 0.5 * DEG,
    },
    Station {
        name: "TERMINATOR",
        focus: Focus::Earth,
        // Side-on to the day/night line.
        yaw_offset: 90.0 * DEG,
        pitch: 14.0 * DEG,
        relative_to: Bearing::SunFromEarth,
        radii: 3.4,
        dwell_s: 12.0,
        drift: 1.1 * DEG,
    },
    Station {
        name: "POLAR SUN",
        focus: Focus::Sun,
        yaw_offset: 40.0 * DEG,
        pitch: 78.0 * DEG,
        relative_to: Bearing::EarthFromSun,
        radii: 7.0,
        dwell_s: 12.0,
        drift: 1.6 * DEG,
    },
    Station {
        name: "LUNAR DIAGONAL",
        focus: Focus::EarthMoon,
        yaw_offset: 55.0 * DEG,
        pitch: 34.0 * DEG,
        relative_to: Bearing::MoonFromEarth,
        radii: 2.6,
        dwell_s: 10.0,
        drift: 1.3 * DEG,
    },
    Station {
        name: "SOLAR VANTAGE",
        focus: Focus::Earth,
        // From out by the Sun, looking back at the Earth.
        yaw_offset: 0.0,
        pitch: 3.0 * DEG,
        relative_to: Bearing::SunFromEarth,
        radii: 90.0,
        dwell_s: 12.0,
        drift: 0.35 * DEG,
    },
    Station {
        name: "INNER SYSTEM",
        focus: Focus::Sun,
        yaw_offset: 25.0 * DEG,
        pitch: 34.0 * DEG,
        relative_to: Bearing::EarthFromSun,
        radii: 340.0,
        dwell_s: 12.0,
        drift: 0.8 * DEG,
    },
];

/// Ease between stations. Long enough to read as a camera move, short enough
/// not to be most of the loop.
pub const TRANSITION_S: f32 = 3.2;

// ── The contact being worked ────────────────────────────────────────────────

/// The two ends of a contact to frame: the operator's QTH and the station being
/// worked, each (latitude, longitude) in degrees.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct QsoPath {
    pub home: (f64, f64),
    pub dx: (f64, f64),
}

/// How far off vertical the camera looks down on the arc's midpoint.
///
/// Straight overhead — 0° — is the worst possible view of a path: the arc's
/// rise is then entirely along the line of sight, so it collapses onto the
/// surface, and the globe reads as a flat disc. Fully side-on, 90°, shows the
/// rise at its full height but leaves both stations sitting exactly on the
/// limb. This sits nearer the side-on end: a shallow enough angle that the
/// horizon curves visibly across the frame and the arc plainly springs off the
/// surface, while both ends stay inside the limb.
const QSO_TILT: f32 = 62.0 * DEG;

/// Fraction of the frame's half-height the framed path may fill. The remainder
/// is margin — which is what keeps the two station markers clear of the edges.
const QSO_FILL: f32 = 0.66;

/// Closest the contact view may get, as a multiple of the Earth's rendered
/// radius. Two stations a few hundred kilometres apart would otherwise pull the
/// camera down to a patch of surface with no horizon in it at all.
const QSO_MIN_DIST: f32 = 0.6;

/// Dwell sway for the contact view: amplitude, and phase rate in radians per
/// second (a round trip every ~24 s).
///
/// The stations drift by a constant yaw, which is fine for a fixed dwell. A
/// contact is held for as long as it lasts, so a constant drift would walk the
/// path out of the frame; swaying keeps the shot alive without leaving it.
const QSO_SWAY: f32 = 5.0 * DEG;
const QSO_SWAY_RATE: f32 = 0.26;

/// Camera pose and pivot that frame a contact: both stations and the arc
/// between them, seen from off to one side of the great circle they lie on.
///
/// `None` when the two ends coincide — which is also when no arc is drawn.
pub fn qso_frame(p: QsoPath, b: &Bodies) -> Option<(Pose, (V3, f32))> {
    let a = b.surface_dir(p.home.0, p.home.1);
    let c = b.surface_dir(p.dx.0, p.dx.1);
    let omega = a.dot(c).clamp(-1.0, 1.0).acos();
    if !omega.is_finite() || omega < 1e-3 {
        return None;
    }

    // The plane the path lies in. Both of these degenerate for an exactly
    // antipodal pair — which has no unique great circle between its ends — so
    // each falls back to an arbitrary but well-defined choice rather than
    // handing the camera a normalised zero vector.
    let n = a.cross(c);
    let normal = if n.len() > 1e-5 { n.normalize() } else { any_perp(a) };
    let s = a + c;
    // Midpoint of the arc: the direction its apex sits over.
    let mid = if s.len() > 1e-5 { s.normalize() } else { normal.cross(a).normalize() };

    let bulge = super::scene::arc_bulge(omega as f64);
    // Pivot half way up the bulge, so the arc's peak and the two ends it
    // springs from straddle the centre of the frame rather than one of them
    // taking it.
    let pivot = b.earth + mid * (b.earth_r * (1.0 + bulge * 0.5));
    // Tilting towards the plane's *normal* is what keeps the two ends
    // equidistant from the view axis: they sit either side of the frame's
    // centre however long the path is.
    let dir = mid * QSO_TILT.cos() + normal * QSO_TILT.sin();

    // Everything the shot has to hold: the two stations, and the top of the arc.
    let span = [a * b.earth_r, c * b.earth_r, mid * (b.earth_r * (1.0 + bulge))]
        .iter()
        .map(|p| (b.earth + *p - pivot).len())
        .fold(0.0f32, f32::max);
    let dist = (span / ((FOV_Y * 0.5).tan() * QSO_FILL)).max(b.earth_r * QSO_MIN_DIST);

    Some((
        Pose::new(dir.y.atan2(dir.x), dir.z.clamp(-1.0, 1.0).asin(), dist),
        // The clamp radius is the size of what is being framed rather than the
        // Earth's, or a short hop could never be approached closely enough to
        // see.
        (pivot, span.max(b.earth_r * 0.15)),
    ))
}

/// Some unit vector perpendicular to `v`, for the cases where the geometry
/// itself singles none out.
fn any_perp(v: V3) -> V3 {
    let alt = if v.z.abs() < 0.9 { v3(0.0, 0.0, 1.0) } else { v3(1.0, 0.0, 0.0) };
    v.cross(alt).normalize()
}

/// A camera pose in the space the tour interpolates.
///
/// Distance is carried as its **logarithm**: a linear ramp across a
/// hundredfold zoom spends nearly all its time at the far end and then lurches,
/// whereas equal steps in log space read as a constant rate of approach.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pose {
    pub yaw: f32,
    pub pitch: f32,
    pub ln_dist: f32,
}

impl Pose {
    fn new(yaw: f32, pitch: f32, dist: f32) -> Pose {
        Pose { yaw, pitch, ln_dist: dist.max(1e-6).ln() }
    }

    fn apply(self, view: &mut crate::view::Solar3dView) {
        view.yaw = self.yaw;
        view.pitch = self.pitch.clamp(-PITCH_LIMIT, PITCH_LIMIT);
        view.dist = self.ln_dist.exp().clamp(1e-5, MAX_DIST);
    }

    fn scaled(self, k: f32) -> Pose {
        Pose { yaw: self.yaw * k, pitch: self.pitch * k, ln_dist: self.ln_dist * k }
    }

    fn plus(self, o: Pose) -> Pose {
        Pose {
            yaw: self.yaw + o.yaw,
            pitch: self.pitch + o.pitch,
            ln_dist: self.ln_dist + o.ln_dist,
        }
    }
}

/// Uniform Catmull-Rom through `p1` and `p2`, shaped by their neighbours.
///
/// This is what makes the tour read as one continuous camera move rather than a
/// series of separate ones: the curve arrives at each station already heading
/// towards the next, so the path bends through the stations instead of forming
/// a corner at each.
fn catmull_rom(p0: Pose, p1: Pose, p2: Pose, p3: Pose, t: f32) -> Pose {
    let t2 = t * t;
    let t3 = t2 * t;
    // 0.5 · [ 2p1 + (−p0+p2)t + (2p0−5p1+4p2−p3)t² + (−p0+3p1−3p2+p3)t³ ]
    p1.scaled(2.0)
        .plus(p2.plus(p0.scaled(-1.0)).scaled(t))
        .plus(
            p0.scaled(2.0)
                .plus(p1.scaled(-5.0))
                .plus(p2.scaled(4.0))
                .plus(p3.scaled(-1.0))
                .scaled(t2),
        )
        .plus(p0.scaled(-1.0).plus(p1.scaled(3.0)).plus(p2.scaled(-3.0)).plus(p3).scaled(t3))
        .scaled(0.5)
}

/// `t³(6t² − 15t + 10)` — zero first *and* second derivative at both ends.
///
/// Used to reparameterise time along the spline, so the camera eases out of one
/// station and settles into the next with no visible kick, while still
/// following the spline's curved path in between.
fn smootherstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

/// Shortest signed angular path from `a` to `b`.
fn short_angle(a: f32, b: f32) -> f32 {
    let mut d = (b - a) % std::f32::consts::TAU;
    if d > std::f32::consts::PI {
        d -= std::f32::consts::TAU;
    } else if d < -std::f32::consts::PI {
        d += std::f32::consts::TAU;
    }
    d
}

/// What the camera is currently flying to and holding.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Leg {
    /// The scripted tour; [`Tour::index`] says which station.
    Station,
    /// The contact being worked, which pre-empts the tour for as long as it
    /// lasts.
    Qso,
}

/// Where the tour is, and where the camera was when it started moving.
#[derive(Clone, Copy)]
pub struct Tour {
    /// The station currently being flown to (and then dwelt at).
    pub index: usize,
    /// Seconds since the move to `index` began.
    pub elapsed: f32,
    /// Pose the current move started from — normally the previous station, but
    /// an arbitrary one when the user re-enables AUTO mid-flight.
    from: Pose,
    /// Pivot the current move started from: position and the radius the
    /// distance clamp uses. Interpolated alongside the pose so the camera flies
    /// between bodies instead of jumping to the next one.
    from_focus: (V3, f32),
    /// True when `from` is a station pose, so the spline can use a real
    /// preceding control point instead of a duplicated one.
    from_is_station: bool,
    started: bool,
    /// Set when AUTO is switched back on: the next `step` picks up at the
    /// nearest station instead of flinging the camera across the system.
    resume_pending: bool,
    /// Which leg the camera is on. Switching between them starts a new move.
    leg: Leg,
    /// The pivot the last step handed back, so a switch between legs can fly
    /// from wherever the camera actually is rather than from the station it
    /// happened to be heading for.
    last_pivot: (V3, f32),
}

impl Default for Tour {
    fn default() -> Self {
        Tour {
            index: 0,
            elapsed: 0.0,
            from: Pose { yaw: 0.0, pitch: 0.0, ln_dist: 0.0 },
            from_focus: (V3::ZERO, 1.0),
            from_is_station: false,
            started: false,
            resume_pending: false,
            leg: Leg::Station,
            last_pivot: (V3::ZERO, 1.0),
        }
    }
}

impl Tour {
    pub fn station(&self) -> &'static Station {
        &STATIONS[self.index % STATIONS.len()]
    }

    /// What the readout calls the leg the camera is on.
    pub fn leg_name(&self) -> &'static str {
        match self.leg {
            Leg::Qso => "QSO PATH",
            Leg::Station => self.station().name,
        }
    }

    /// Whether the camera is currently moving rather than holding a station.
    pub fn in_transit(&self) -> bool {
        self.elapsed < TRANSITION_S
    }

    /// Advance the tour and write the camera pose into `view`.
    /// Ask for the tour to pick up near the current view on its next step.
    pub fn request_resume(&mut self) {
        self.resume_pending = true;
    }

    /// Advance the tour. Returns the pivot the camera should use this frame:
    /// during a transition an interpolated point between the two stations'
    /// bodies, and at rest simply the station's own body.
    ///
    /// `qso` is the contact being worked, if any. A contact pre-empts the tour
    /// entirely — the camera flies to it and holds it until the contact ends,
    /// because a path being worked *now* is the one thing on this globe worth
    /// watching more than the scripted loop.
    pub fn step(
        &mut self,
        view: &mut crate::view::Solar3dView,
        b: &Bodies,
        dt: f32,
        qso: Option<QsoPath>,
    ) -> (V3, f32) {
        if std::mem::take(&mut self.resume_pending) {
            self.resume_near(view, b);
        }
        if !self.started {
            self.from = Pose::new(view.yaw, view.pitch, view.dist);
            // Start the flight from whatever the camera was actually looking
            // at, so enabling AUTO mid-view does not jump either.
            self.from_focus = b.focus(Focus::from_u8(view.focus));
            self.last_pivot = self.from_focus;
            self.from_is_station = false;
            self.started = true;
            self.elapsed = 0.0;
        }

        // A contact that has no framing — both ends in the same place — is no
        // contact as far as the camera is concerned; the arc is not drawn for
        // one either.
        let frame = qso.and_then(|p| qso_frame(p, b));
        let want = if frame.is_some() { Leg::Qso } else { Leg::Station };
        if want != self.leg {
            // Hand over from wherever the camera is at this instant, mid-flight
            // or not, so picking up a contact and giving it back are both
            // ordinary camera moves.
            self.from = Pose::new(view.yaw, view.pitch, view.dist);
            self.from_focus = self.last_pivot;
            self.from_is_station = false;
            self.elapsed = 0.0;
            if want == Leg::Station {
                // Rejoin the loop at whatever station is nearest rather than
                // where it was left, which by now may be on the far side of the
                // system.
                self.index = self.nearest_station(view, b);
            }
            self.leg = want;
        }

        // Clamped, so a stalled frame (a resize, a GPU hitch) does not teleport
        // the camera halfway through a move.
        self.elapsed += dt.clamp(0.0, 0.25);

        let station = self.station();
        let (target, target_focus) = match frame {
            Some(f) => f,
            None => (self.pose_of(station, b), b.focus(station.focus)),
        };

        let pivot;
        if self.elapsed < TRANSITION_S {
            // p1 → p2 is this move; p0 and p3 shape its curvature.
            let p1 = self.from;
            let p2 = unwrap_to(p1, target);
            let p0 = if self.from_is_station {
                unwrap_to(p1, self.pose_of(self.station_at(self.index as isize - 2), b))
            } else {
                // Started from a manual pose: no history, so duplicate p1,
                // which gives the spline a zero incoming tangent (it eases out
                // of where the user left the camera).
                p1
            };
            // On the contact leg there is no station after this one to bend
            // towards; duplicating p2 gives a zero outgoing tangent, so the
            // camera settles onto the path instead of sweeping past it.
            let p3 = match frame {
                Some(_) => p2,
                None => unwrap_to(p2, self.pose_of(self.station_at(self.index as isize + 1), b)),
            };
            let k = smootherstep(self.elapsed / TRANSITION_S);
            catmull_rom(p0, p1, p2, p3, k).apply(view);
            // The pivot is eased linearly rather than splined: a Catmull-Rom
            // through Sun→Earth→Sun would overshoot past the bodies, and a
            // camera that flies *beyond* its target and comes back reads as a
            // mistake. Smootherstep still gives zero velocity at both ends.
            pivot = (
                self.from_focus.0 + (target_focus.0 - self.from_focus.0) * k,
                self.from_focus.1 + (target_focus.1 - self.from_focus.1) * k,
            );
        } else {
            // Dwell. A slow drift keeps the frame alive rather than freezing.
            let held = self.elapsed - TRANSITION_S;
            let drift = match frame {
                Some(_) => QSO_SWAY * (held * QSO_SWAY_RATE).sin(),
                None => station.drift * held,
            };
            Pose { yaw: target.yaw + drift, ..target }.apply(view);
            pivot = target_focus;
            // A contact is held for as long as it lasts; only the tour's own
            // stations time out.
            if frame.is_none() && held >= station.dwell_s {
                self.index = (self.index + 1) % STATIONS.len();
                self.from = Pose::new(view.yaw, view.pitch, view.dist);
                // Hand the *current* pivot to the next move, so the flight
                // starts exactly where this one ended.
                self.from_focus = target_focus;
                self.from_is_station = true;
                self.elapsed = 0.0;
            }
        }
        // The contact view pivots around a point above the Earth's surface, but
        // the Earth is what the user gets if they take the controls back.
        view.focus = match frame {
            Some(_) => Focus::Earth.to_u8(),
            None => station.focus.to_u8(),
        };
        self.last_pivot = pivot;
        pivot
    }

    /// Resume at whichever station is closest to the current view, so
    /// re-enabling AUTO does not fling the camera across the system.
    pub fn resume_near(&mut self, view: &crate::view::Solar3dView, b: &Bodies) {
        self.index = self.nearest_station(view, b);
        self.started = false;
    }

    /// The station the camera would have the shortest flight to from here.
    fn nearest_station(&self, view: &crate::view::Solar3dView, b: &Bodies) -> usize {
        let here = Pose::new(view.yaw, view.pitch, view.dist);
        let mut best = (f32::MAX, 0usize);
        for (i, s) in STATIONS.iter().enumerate() {
            let p = self.pose_of(s, b);
            let cost = short_angle(here.yaw, p.yaw).abs()
                + (p.pitch - here.pitch).abs()
                + (p.ln_dist - here.ln_dist).abs();
            if cost < best.0 {
                best = (cost, i);
            }
        }
        best.1
    }

    fn station_at(&self, i: isize) -> &'static Station {
        let n = STATIONS.len() as isize;
        &STATIONS[(i.rem_euclid(n)) as usize]
    }

    fn pose_of(&self, s: &Station, b: &Bodies) -> Pose {
        let bearing = match s.relative_to {
            Bearing::World => 0.0,
            Bearing::SunFromEarth => yaw_of(V3::ZERO - b.earth),
            Bearing::EarthFromSun => yaw_of(b.earth),
            Bearing::MoonFromEarth => yaw_of(b.moon - b.earth),
        };
        let (_, radius) = b.focus(s.focus);
        let (lo, hi) = dist_range(radius);
        Pose::new(
            bearing + s.yaw_offset,
            s.pitch.clamp(-PITCH_LIMIT, PITCH_LIMIT),
            (radius * s.radii).clamp(lo, hi),
        )
    }
}

/// Re-express `p`'s yaw as the branch nearest `near`.
///
/// The spline is evaluated on raw numbers, so every control point has to be on
/// one continuous branch first — otherwise a pair straddling ±π sends the
/// camera the long way round, or worse, spinning.
fn unwrap_to(near: Pose, p: Pose) -> Pose {
    Pose { yaw: near.yaw + short_angle(near.yaw, p.yaw), ..p }
}

/// Bearing of a direction in the ecliptic plane.
fn yaw_of(v: V3) -> f32 {
    v.y.atan2(v.x)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::solar3d::scene;
    use crate::view::Solar3dView;

    /// The camera, its bodies, and the point it is pivoting around.
    fn cam_at(dist: f32, yaw: f32, pitch: f32) -> (Camera, Bodies, V3) {
        let mut st = SolarUi::new(Solar3dView::default());
        st.view.dist = dist;
        st.view.yaw = yaw;
        st.view.pitch = pitch;
        let b = scene::bodies(&st, 1_784_937_600.0);
        let focus = b.focus(st.focus()).0;
        (Camera::from_view(&st, &b, [1600.0, 900.0]), b, focus)
    }

    #[test]
    fn the_eye_sits_at_the_requested_distance_from_the_focus() {
        let (c, _, focus) = cam_at(300.0, 0.6, 0.55);
        assert!(((c.eye - focus).len() - 300.0).abs() < 1e-2);
        // Positive pitch looks down from above the ecliptic.
        assert!(c.eye.z > 0.0);
    }

    #[test]
    fn distance_is_clamped_outside_the_focused_body() {
        let mut st = SolarUi::new(Solar3dView::default());
        st.view.focus = crate::solar3d::state::Focus::Earth.to_u8();
        st.view.dist = 1e-9;
        let b = scene::bodies(&st, 1_784_937_600.0);
        let c = Camera::from_view(&st, &b, [800.0, 600.0]);
        let focus = b.focus(st.focus()).0;
        assert!((c.eye - focus).len() > b.earth_r, "camera ended up inside the Earth");
    }

    #[test]
    fn the_focus_projects_to_the_centre_of_the_screen() {
        let (c, _, focus) = cam_at(300.0, 1.1, -0.3);
        let m = &c.view_proj;
        let p = focus;
        let mut o = [0.0f32; 4];
        for (r, out) in o.iter_mut().enumerate() {
            *out = m.cols[0][r] * p.x + m.cols[1][r] * p.y + m.cols[2][r] * p.z + m.cols[3][r];
        }
        assert!(o[3] > 0.0, "focus behind the camera");
        assert!((o[0] / o[3]).abs() < 1e-4 && (o[1] / o[3]).abs() < 1e-4, "focus at {o:?}");
    }

    #[test]
    fn apparent_size_falls_off_with_distance() {
        let (near, b, _) = cam_at(2.0, 0.0, 0.0);
        let (far, _, _) = cam_at(20.0, 0.0, 0.0);
        let a = near.pixels_for(V3::ZERO, b.sun_r);
        let z = far.pixels_for(V3::ZERO, b.sun_r);
        assert!(a > z * 5.0, "{a} px at 2 Gm vs {z} px at 20 Gm");
    }

    // ── The AUTO tour ───────────────────────────────────────────────────────

    fn pose(yaw: f32, pitch: f32, ln_dist: f32) -> Pose {
        Pose { yaw, pitch, ln_dist }
    }

    #[test]
    fn catmull_rom_passes_through_its_middle_control_points() {
        let (p0, p1) = (pose(0.0, 0.0, 0.0), pose(1.0, 2.0, 3.0));
        let (p2, p3) = (pose(4.0, 1.0, 5.0), pose(9.0, -1.0, 2.0));
        let a = catmull_rom(p0, p1, p2, p3, 0.0);
        let b = catmull_rom(p0, p1, p2, p3, 1.0);
        for (got, want) in [(a, p1), (b, p2)] {
            assert!((got.yaw - want.yaw).abs() < 1e-5, "{got:?} vs {want:?}");
            assert!((got.pitch - want.pitch).abs() < 1e-5);
            assert!((got.ln_dist - want.ln_dist).abs() < 1e-5);
        }
    }

    /// The spline must actually curve: a straight lerp between the same two
    /// points would sit exactly on the chord, and this asserts it does not.
    #[test]
    fn catmull_rom_bends_towards_its_neighbours() {
        let (p0, p1) = (pose(0.0, 0.0, 0.0), pose(1.0, 0.0, 0.0));
        let (p2, p3) = (pose(2.0, 0.0, 0.0), pose(3.0, 4.0, 0.0));
        let mid = catmull_rom(p0, p1, p2, p3, 0.5);
        let chord_pitch = 0.0; // p1.pitch and p2.pitch are both zero
        assert!(
            (mid.pitch - chord_pitch).abs() > 0.05,
            "midpoint pitch {} sits on the chord — this is a lerp, not a spline",
            mid.pitch
        );
    }

    fn run_tour(seconds: f32, dt: f32) -> (Vec<Pose>, Vec<&'static str>) {
        let (poses, names, _) = run_tour_eyes(seconds, dt);
        (poses, names)
    }

    /// Run the tour, recording the pose, the station sequence, and — crucially —
    /// the resulting **eye position**, which is what the viewer actually sees
    /// move.
    fn run_tour_eyes(seconds: f32, dt: f32) -> (Vec<Pose>, Vec<&'static str>, Vec<V3>) {
        let mut st = SolarUi::new(Solar3dView::default());
        st.view.auto = true;
        let b = scene::bodies(&st, 1_784_937_600.0);
        let mut tour = Tour::default();
        let (mut poses, mut names, mut eyes) = (Vec::new(), Vec::new(), Vec::new());
        for _ in 0..(seconds / dt) as usize {
            let pivot = tour.step(&mut st.view, &b, dt, None);
            st.focus_override = Some(pivot);
            poses.push(Pose::new(st.view.yaw, st.view.pitch, st.view.dist));
            eyes.push(Camera::from_view(&st, &b, [1600.0, 900.0]).eye);
            let n = tour.station().name;
            if names.last() != Some(&n) {
                names.push(n);
            }
        }
        (poses, names, eyes)
    }

    /// The whole point of the spline: the camera must never jump. Sampled at
    /// 60 fps across a full loop, every per-frame step in yaw, pitch and log
    /// distance has to stay small.
    #[test]
    fn the_tour_path_is_continuous() {
        let dt = 1.0 / 60.0;
        let (poses, _) = run_tour(140.0, dt);
        assert!(poses.len() > 8000);
        let mut worst = (0.0f32, 0usize);
        for (i, w) in poses.windows(2).enumerate() {
            let dyaw = short_angle(w[0].yaw, w[1].yaw).abs();
            let dpitch = (w[1].pitch - w[0].pitch).abs();
            let ddist = (w[1].ln_dist - w[0].ln_dist).abs();
            let step = dyaw.max(dpitch).max(ddist);
            if step > worst.0 {
                worst = (step, i);
            }
        }
        // A frame of the fastest move covers a few degrees at most; anything
        // approaching a radian is a visible snap.
        assert!(
            worst.0 < 0.12,
            "frame {} jumped by {} (rad or ln-units) — the path is not smooth",
            worst.1,
            worst.0
        );
    }

    /// ...and it must be smooth in acceleration too, or the moves read as
    /// starting and stopping abruptly even though the positions are continuous.
    #[test]
    fn the_tour_has_no_velocity_discontinuities() {
        let dt = 1.0 / 60.0;
        let (poses, _) = run_tour(140.0, dt);
        let vel: Vec<f32> = poses.windows(2).map(|w| short_angle(w[0].yaw, w[1].yaw)).collect();
        let worst = vel.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        assert!(worst < 0.02, "yaw acceleration spike of {worst} rad/frame²");
    }

    /// The test that matters: the **eye** must not jump.
    ///
    /// Yaw, pitch and distance being continuous is not sufficient. The camera
    /// pivots around a body, and stations focus different bodies — so switching
    /// focus at the start of a transition teleports the eye by up to 1 AU while
    /// every interpolated value stays perfectly smooth. Measuring the pose alone
    /// misses that completely.
    #[test]
    fn the_eye_never_jumps_between_stations() {
        let dt = 1.0 / 60.0;
        let (_, _, eyes) = run_tour_eyes(160.0, dt);
        assert!(eyes.len() > 9000);

        // The criterion is the *smoothness* of the step profile, not its
        // magnitude: the tour deliberately crosses 200 Gm in 3.2 s, so a 7 Gm
        // frame is normal mid-flight. A teleport is instead one enormous step
        // between two tiny ones, so it shows up as the step size changing by
        // about its own size from one frame to the next.
        let steps: Vec<f32> = eyes.windows(2).map(|w| (w[1] - w[0]).len()).collect();
        let peak = steps.iter().copied().fold(0.0f32, f32::max);
        let (jerk, at) = steps
            .windows(2)
            .enumerate()
            .map(|(i, w)| ((w[1] - w[0]).abs(), i))
            .fold((0.0f32, 0usize), |acc, x| if x.0 > acc.0 { x } else { acc });
        // A smooth ease measures about 0.025; an instant focus switch is ~1.0.
        assert!(
            jerk < peak * 0.15,
            "eye step changed by {jerk} Gm at frame {at} against a {peak} Gm peak \
             ({:.2} of it) — that is a jump, not a flight",
            jerk / peak
        );

        // And nothing pathological: no NaNs, nothing flung outside the system.
        for e in &eyes {
            assert!(e.x.is_finite() && e.y.is_finite() && e.z.is_finite());
            assert!(e.len() < 3000.0, "eye at {} Gm from the Sun", e.len());
        }
    }

    /// A focus change on its own — the exact case the pivot interpolation
    /// exists for. Stepping from a Sun-focused station to an Earth-focused one
    /// must move the pivot gradually, not snap it 1 AU.
    #[test]
    fn the_pivot_eases_between_bodies() {
        let mut st = SolarUi::new(Solar3dView::default());
        st.view.auto = true;
        let b = scene::bodies(&st, 1_784_937_600.0);
        let mut tour = Tour::default();

        // Settle at station 0 (Sun), then advance to station 1 (Earth).
        let mut pivot = tour.step(&mut st.view, &b, 1.0 / 60.0, None);
        while tour.index == 0 {
            pivot = tour.step(&mut st.view, &b, 0.05, None);
        }
        assert_eq!(tour.station().focus, Focus::Earth);
        // The first frame of the new move must still be at the Sun, not at the
        // Earth: that is the jump this guards against.
        assert!(pivot.0.len() < 1.0, "pivot already {} Gm out", pivot.0.len());

        let mut prev = pivot.0;
        let mut worst = 0.0f32;
        for _ in 0..(TRANSITION_S / 0.02) as usize + 4 {
            let p = tour.step(&mut st.view, &b, 0.02, None).0;
            worst = worst.max((p - prev).len());
            prev = p;
        }
        // Arrived at the Earth...
        assert!(
            (prev - b.earth).len() < 0.01,
            "pivot ended {} Gm off the Earth",
            (prev - b.earth).len()
        );
        // ...having covered 1 AU in steps no larger than a smooth ease implies.
        assert!(worst < 3.0, "pivot moved {worst} Gm in a single 20 ms step");
    }

    #[test]
    fn the_tour_visits_every_station_and_loops() {
        // One full loop is ~8 stations × (3.2 s transition + ~12 s dwell).
        let (_, names) = run_tour(160.0, 1.0 / 30.0);
        let mut seen: Vec<&str> = Vec::new();
        for n in &names {
            if !seen.contains(n) {
                seen.push(n);
            }
        }
        assert_eq!(seen.len(), STATIONS.len(), "only visited {seen:?}");
        // Order is the table's order, and it wraps.
        assert_eq!(names[0], STATIONS[0].name);
        assert_eq!(names[1], STATIONS[1].name);
    }

    #[test]
    fn the_tour_stays_within_the_camera_limits() {
        let (poses, _) = run_tour(160.0, 1.0 / 30.0);
        for p in &poses {
            assert!(p.pitch.abs() <= PITCH_LIMIT + 1e-4, "pitch {} out of range", p.pitch);
            let d = p.ln_dist.exp();
            assert!(d > 0.0 && d <= MAX_DIST + 1.0, "distance {d} out of range");
            assert!(p.yaw.is_finite() && p.pitch.is_finite() && p.ln_dist.is_finite());
        }
    }

    /// Re-enabling AUTO after the user has moved the camera must pick the
    /// nearest station, not restart the loop from the beginning.
    #[test]
    fn resuming_picks_up_at_the_nearest_station() {
        let mut st = SolarUi::new(Solar3dView::default());
        let b = scene::bodies(&st, 1_784_937_600.0);
        let mut tour = Tour::default();

        // Park the camera at station 4's pose, then resume.
        let target = tour.pose_of(&STATIONS[4], &b);
        st.view.yaw = target.yaw;
        st.view.pitch = target.pitch;
        st.view.dist = target.ln_dist.exp();
        tour.index = 0;
        tour.request_resume();
        tour.step(&mut st.view, &b, 1.0 / 60.0, None);
        assert_eq!(tour.index, 4, "resumed at {} instead", tour.station().name);
    }

    /// A stalled frame must not teleport the camera: the step is clamped, so a
    /// one-second hitch advances the move by at most a quarter second.
    #[test]
    fn a_long_frame_does_not_teleport_the_camera() {
        let mut st = SolarUi::new(Solar3dView::default());
        let b = scene::bodies(&st, 1_784_937_600.0);
        let mut tour = Tour::default();
        tour.step(&mut st.view, &b, 1.0 / 60.0, None);
        let before = Pose::new(st.view.yaw, st.view.pitch, st.view.dist);
        tour.step(&mut st.view, &b, 5.0, None);
        let after = Pose::new(st.view.yaw, st.view.pitch, st.view.dist);
        assert!(tour.elapsed <= 0.3, "elapsed jumped to {}", tour.elapsed);
        assert!(
            short_angle(before.yaw, after.yaw).abs() < 0.5,
            "camera jumped {} rad on a stalled frame",
            short_angle(before.yaw, after.yaw).abs()
        );
    }

    #[test]
    fn unwrapping_keeps_the_spline_on_one_branch() {
        let near = pose(3.0, 0.0, 0.0);
        // 3.0 and −3.0 are 0.28 rad apart the short way, 6 rad apart the long way.
        let far = unwrap_to(near, pose(-3.0, 0.0, 0.0));
        assert!((far.yaw - near.yaw).abs() < 0.4, "unwrapped to {}", far.yaw);
        assert!(far.yaw > std::f32::consts::PI, "took the long way: {}", far.yaw);
    }

    // ── The contact view ────────────────────────────────────────────────────

    /// A path of `sep` degrees, from a mid-latitude QTH along a meridian.
    fn path_of(sep: f64) -> QsoPath {
        QsoPath { home: (48.2, 16.4), dx: (48.2 - sep, 16.4) }
    }

    /// Fly the tour with a contact in progress until it has settled on it.
    fn settled_on(path: QsoPath) -> (SolarUi, Bodies, Camera) {
        let mut st = SolarUi::new(Solar3dView::default());
        st.view.auto = true;
        let b = scene::bodies(&st, 1_784_937_600.0);
        let mut tour = Tour::default();
        let dt = 1.0 / 60.0;
        // Long enough for the flight in (3.2 s) and a good part of the sway.
        for _ in 0..(12.0 / dt) as usize {
            let pivot = tour.step(&mut st.view, &b, dt, Some(path));
            st.focus_override = Some(pivot);
        }
        assert_eq!(tour.leg_name(), "QSO PATH");
        let cam = Camera::from_view(&st, &b, [1600.0, 900.0]);
        (st, b, cam)
    }

    /// Normalised device coordinates, or `None` when the point is behind the
    /// camera. The viewport is everything within ±1 on both axes.
    fn ndc(cam: &Camera, p: V3) -> Option<(f32, f32)> {
        let m = &cam.view_proj;
        let mut o = [0.0f32; 4];
        for (r, out) in o.iter_mut().enumerate() {
            *out = m.cols[0][r] * p.x + m.cols[1][r] * p.y + m.cols[2][r] * p.z + m.cols[3][r];
        }
        (o[3] > 0.0).then(|| (o[0] / o[3], o[1] / o[3]))
    }

    /// A point on (or above) the globe, the way the arc is drawn.
    fn on_globe(b: &Bodies, lat: f64, lon: f64, lift: f32) -> V3 {
        b.earth + b.surface_dir(lat, lon) * (b.earth_r * lift)
    }

    /// Whether any part of the Earth's horizon is in the frame — the test of
    /// "the curvature is visible" that survives the camera being close enough
    /// for the globe to overflow the viewport.
    fn limb_on_screen(cam: &Camera, b: &Bodies) -> bool {
        let d = (cam.eye - b.earth).len();
        let u = (cam.eye - b.earth).normalize();
        let (e1, e2) = (any_perp(u), u.cross(any_perp(u)));
        // The horizon is the circle at this angle from the sub-camera point.
        let alpha = (b.earth_r / d).clamp(-1.0, 1.0).acos();
        (0..180).any(|k| {
            let phi = k as f32 / 180.0 * std::f32::consts::TAU;
            let dir = u * alpha.cos() + (e1 * phi.cos() + e2 * phi.sin()) * alpha.sin();
            ndc(cam, b.earth + dir * b.earth_r)
                .is_some_and(|(x, y)| x.abs() <= 1.0 && y.abs() <= 1.0)
        })
    }

    /// The point of the whole thing: both stations and the arc between them
    /// have to be on screen, at every path length from a local contact to a
    /// near-antipodal one.
    #[test]
    fn the_contact_view_frames_both_stations_and_the_arc() {
        for sep in [1.0, 12.0, 60.0, 120.0, 178.0] {
            let path = path_of(sep);
            let (_, b, cam) = settled_on(path);
            let ends = [path.home, path.dx].map(|(lat, lon)| {
                ndc(&cam, on_globe(&b, lat, lon, 1.0))
                    .unwrap_or_else(|| panic!("{sep}° path: an end is behind the camera"))
            });
            for (i, (x, y)) in ends.iter().enumerate() {
                assert!(
                    x.abs() < 0.98 && y.abs() < 0.98,
                    "{sep}° path: end {i} is off screen at ({x:.2}, {y:.2})",
                );
            }
            // ...and framed, not merely present: a shot where the two ends sit
            // on top of each other has not zoomed in on anything. Only a path
            // long enough to *have* a shape gets held to the tighter bound —
            // two stations 100 km apart are close together on any globe with a
            // horizon still in it.
            let gap = ((ends[0].0 - ends[1].0).powi(2) + (ends[0].1 - ends[1].1).powi(2)).sqrt();
            let want = if sep >= 12.0 { 0.35 } else { 0.03 };
            assert!(gap > want, "{sep}° path: the two ends are {gap:.3} apart on screen");
        }
    }

    /// Shallow enough that the globe reads as a sphere and the arc's rise is
    /// plainly visible, rather than the overhead view that flattens both.
    #[test]
    fn the_contact_view_is_a_shallow_one() {
        for sep in [12.0, 60.0, 120.0] {
            let path = path_of(sep);
            let (st, b, cam) = settled_on(path);
            let pivot = st.focus_override.expect("the tour supplies a pivot").0;

            // How far the line of sight is off the vertical where it lands.
            // Straight down is 0°, along the surface is 90°.
            let vertical = (pivot - b.earth).normalize();
            let ray = (cam.eye - pivot).normalize();
            let tilt = ray.dot(vertical).clamp(-1.0, 1.0).acos() / DEG;
            assert!(
                (40.0..80.0).contains(&tilt),
                "{sep}° path: looking down at {tilt:.0}° off the vertical",
            );
            // Shallow is only half of it: the horizon has to be in the frame,
            // or there is no curvature on screen to see.
            assert!(limb_on_screen(&cam, &b), "{sep}° path: no horizon in the frame");

            // And the rise itself: the apex of the arc must project clear of
            // the surface directly below it, which is exactly what an overhead
            // shot loses.
            let omega = (sep as f32) * DEG;
            let bulge = scene::arc_bulge(omega as f64);
            let (mlat, mlon) = (path.home.0 - sep * 0.5, path.home.1);
            let apex = ndc(&cam, on_globe(&b, mlat, mlon, 1.0 + bulge)).expect("apex on screen");
            let below = ndc(&cam, on_globe(&b, mlat, mlon, 1.0)).expect("surface on screen");
            let rise = ((apex.0 - below.0).powi(2) + (apex.1 - below.1).powi(2)).sqrt();
            assert!(rise > 0.06, "{sep}° path: the arc rises only {rise:.3} on screen");
        }
    }

    /// The same, for paths that lie the way real ones do rather than along a
    /// meridian: the framing is built on the plane of the path, so a path over
    /// the pole and one along the equator put the camera in quite different
    /// places relative to the ecliptic the camera's "up" is fixed to.
    #[test]
    fn the_contact_view_holds_up_for_real_paths() {
        let vienna = (48.2, 16.4);
        for (what, path) in [
            ("Vienna–Tokyo", QsoPath { home: vienna, dx: (35.7, 139.7) }),
            ("Vienna–Sydney", QsoPath { home: vienna, dx: (-33.9, 151.2) }),
            ("Vienna–Buenos Aires", QsoPath { home: vienna, dx: (-34.6, -58.4) }),
            // Along the equator: the plane of the path is the one furthest
            // from the ecliptic the camera keeps level.
            ("Nairobi–Singapore", QsoPath { home: (-1.3, 36.8), dx: (1.4, 103.8) }),
            // And straight over the pole.
            ("Reykjavik–Anchorage", QsoPath { home: (64.1, -21.9), dx: (61.2, -149.9) }),
        ] {
            let (st, b, cam) = settled_on(path);
            let ends = [path.home, path.dx].map(|(lat, lon)| {
                ndc(&cam, on_globe(&b, lat, lon, 1.0))
                    .unwrap_or_else(|| panic!("{what}: an end is behind the camera"))
            });
            for (i, (x, y)) in ends.iter().enumerate() {
                assert!(x.abs() < 0.98 && y.abs() < 0.98, "{what}: end {i} at ({x:.2}, {y:.2})");
            }
            let gap = ((ends[0].0 - ends[1].0).powi(2) + (ends[0].1 - ends[1].1).powi(2)).sqrt();
            assert!(gap > 0.35, "{what}: the two ends are {gap:.3} apart on screen");

            let pivot = st.focus_override.expect("the tour supplies a pivot").0;
            let tilt = (cam.eye - pivot)
                .normalize()
                .dot((pivot - b.earth).normalize())
                .clamp(-1.0, 1.0)
                .acos()
                / DEG;
            assert!((40.0..80.0).contains(&tilt), "{what}: {tilt:.0}° off the vertical");
            assert!(limb_on_screen(&cam, &b), "{what}: no horizon in the frame");
            assert!((cam.eye - b.earth).len() > b.earth_r * 1.05, "{what}: eye inside the globe");
        }
    }

    /// However the path lies, the camera must stay out in space — a pivot that
    /// sits just above the surface makes this easy to get wrong.
    #[test]
    fn the_contact_view_stays_outside_the_globe() {
        for sep in [0.5, 5.0, 45.0, 90.0, 150.0, 179.9] {
            let path = path_of(sep);
            let (_, b, cam) = settled_on(path);
            let h = (cam.eye - b.earth).len();
            assert!(h > b.earth_r * 1.05, "{sep}° path: eye {h} inside a {} globe", b.earth_r);
            assert!(cam.eye.x.is_finite() && cam.eye.y.is_finite() && cam.eye.z.is_finite());
        }
    }

    /// Two stations in the same place have no path between them and no arc is
    /// drawn for them either, so the tour must carry on rather than framing a
    /// point.
    #[test]
    fn a_contact_with_itself_is_not_framed() {
        let mut st = SolarUi::new(Solar3dView::default());
        let b = scene::bodies(&st, 1_784_937_600.0);
        assert!(qso_frame(QsoPath { home: (48.2, 16.4), dx: (48.2, 16.4) }, &b).is_none());

        let mut tour = Tour::default();
        let path = QsoPath { home: (48.2, 16.4), dx: (48.200_01, 16.400_01) };
        for _ in 0..600 {
            tour.step(&mut st.view, &b, 1.0 / 60.0, Some(path));
        }
        assert_ne!(tour.leg_name(), "QSO PATH");
    }

    /// A contact takes the camera over for as long as it lasts — well past the
    /// dwell any station gets — and hands it back when it ends. Neither
    /// handover may jump the eye.
    #[test]
    fn a_contact_pre_empts_the_tour_and_hands_it_back() {
        let dt = 1.0 / 60.0;
        let mut st = SolarUi::new(Solar3dView::default());
        st.view.auto = true;
        let b = scene::bodies(&st, 1_784_937_600.0);
        let mut tour = Tour::default();
        let path = path_of(75.0);

        let mut eyes: Vec<V3> = Vec::new();
        let frame = |tour: &mut Tour, st: &mut SolarUi, qso, eyes: &mut Vec<V3>| {
            let pivot = tour.step(&mut st.view, &b, dt, qso);
            st.focus_override = Some(pivot);
            eyes.push(Camera::from_view(st, &b, [1600.0, 900.0]).eye);
        };

        // Settle into the tour first, so the contact arrives mid-dwell.
        for _ in 0..(10.0 / dt) as usize {
            frame(&mut tour, &mut st, None, &mut eyes);
        }
        assert!(!tour.in_transit(), "the tour should be dwelling by now");
        let at_pickup = eyes.len();

        // The contact runs for 40 s — longer than any station's dwell.
        for _ in 0..(40.0 / dt) as usize {
            frame(&mut tour, &mut st, Some(path), &mut eyes);
        }
        assert_eq!(tour.leg_name(), "QSO PATH", "the tour walked off the contact");
        let (target, pivot) = qso_frame(path, &b).expect("a 75° path is framable");
        assert!(
            short_angle(st.view.yaw, target.yaw).abs() < QSO_SWAY * 1.1,
            "settled {} rad off the framing",
            short_angle(st.view.yaw, target.yaw).abs(),
        );
        assert!((st.view.dist - target.ln_dist.exp()).abs() < target.ln_dist.exp() * 0.02);
        assert!((st.focus_override.expect("pivot").0 - pivot.0).len() < b.earth_r * 0.01);
        let at_handback = eyes.len();

        // Contact over: the tour takes back over.
        for _ in 0..(20.0 / dt) as usize {
            frame(&mut tour, &mut st, None, &mut eyes);
        }
        assert_ne!(tour.leg_name(), "QSO PATH", "the camera never left the contact");

        // Both handovers start a flight from rest, so the frame either side of
        // one may not move further than the frame before it did.
        for (what, i) in [("pick-up", at_pickup), ("hand-back", at_handback)] {
            let before = (eyes[i - 1] - eyes[i - 2]).len();
            let after = (eyes[i] - eyes[i - 1]).len();
            assert!(
                after <= before * 2.0 + 1e-6,
                "{what}: the eye jumped {after} Gm against {before} Gm the frame before",
            );
        }
        for e in &eyes {
            assert!(e.x.is_finite() && e.y.is_finite() && e.z.is_finite());
        }
    }
}
