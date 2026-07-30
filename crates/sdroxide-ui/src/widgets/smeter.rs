//! S-meter with three selectable faces: an analog needle instrument (the
//! default), a horizontal bar, and a scrolling trace of the last fifteen
//! seconds. All three are clickable — the caller cycles the face on a click —
//! and all three switch to a TX meter while transmitting, where the headline
//! reading is SWR on its own logarithmic scale, drawn as a real meter rather
//! than a number.
//!
//! The three share their scales, colour ramps and header, so a reading looks
//! the same wherever it is shown; only the visualisation differs.
//!
//! The meter fills its whole box edge-to-edge: it paints its own instrument
//! face over the full allocated rect and draws the box border on top, so there
//! is no padding between the instrument and the frame.
//!
//! Everything is laid out against `k`, the box height over its 72 px design
//! height, so the whole instrument scales with the box it is handed.

use eframe::egui::{
    Align2, Color32, FontId, Id, Painter, Pos2, Rangef, Rect, Response, Sense, Shape, Stroke,
    StrokeKind, Ui, Vec2,
    epaint::{Mesh, Vertex, WHITE_UV},
    pos2, vec2,
};
use sdroxide_types::Meters;

/// Scale endpoints: S0 = -127 dBm … S9+60 = -13 dBm.
const DBM_LO: f32 = -127.0;
const DBM_HI: f32 = -13.0;
const S9_DBM: f32 = -73.0;

/// SWR at the top of the SWR scale. With 3:1 pinned to the halfway point and a
/// logarithmic scale, the end of travel falls out as 3² = 9:1.
const SWR_MAX: f32 = 9.0;

const RED: Color32 = Color32::from_rgb(255, 60, 70);
const CYAN: Color32 = Color32::from_rgb(0, 208, 244);
/// Readout text: the headline value and the smaller sub-reading beside it.
const READOUT: Color32 = Color32::from_rgb(0xe6, 0xf6, 0xff);
const SUBDUED: Color32 = Color32::from_rgb(0x84, 0x9e, 0xb6);
/// Instrument face: a dark navy wash, lighter at the top.
const FACE_TOP: Color32 = Color32::from_rgb(0x11, 0x1b, 0x2b);
const FACE_BOT: Color32 = Color32::from_rgb(0x03, 0x06, 0x0c);
/// The backlight bloom behind the scale, on receive and on transmit.
const BACKLIGHT: Color32 = Color32::from_rgb(0x1c, 0x74, 0x96);
const BACKLIGHT_TX: Color32 = Color32::from_rgb(0x8e, 0x1e, 0x2c);

/// Tick and label inks, cool (below the red-line) and hot (above it).
const TICK_MINOR: [Color32; 2] =
    [Color32::from_rgb(0x54, 0x70, 0x8a), Color32::from_rgb(0x93, 0x4e, 0x52)];
const TICK_MAJOR: [Color32; 2] =
    [Color32::from_rgb(0xcf, 0xe6, 0xf5), Color32::from_rgb(0xff, 0x92, 0x8a)];
const LABEL: [Color32; 2] =
    [Color32::from_rgb(0xbd, 0xd6, 0xe8), Color32::from_rgb(0xff, 0x7d, 0x74)];
/// Trace-view gridlines and the axis labels beside them, cool and hot.
const GRID_LINE: [Color32; 2] =
    [Color32::from_rgb(0x1b, 0x2a, 0x3e), Color32::from_rgb(0x47, 0x23, 0x2b)];
const GRID_LABEL: [Color32; 2] =
    [Color32::from_rgb(0x63, 0x7c, 0x92), Color32::from_rgb(0xbb, 0x60, 0x5d)];

/// Position on the S-scale for `dbm`, 0.0 (S0) … 1.0 (S9+60).
fn frac_of(dbm: f32) -> f32 {
    ((dbm - DBM_LO) / (DBM_HI - DBM_LO)).clamp(0.0, 1.0)
}

/// Position on the SWR scale, 0.0 (1:1) … 1.0 (9:1). Logarithmic, so 3:1 — the
/// point where most operators stop transmitting — lands exactly halfway.
fn swr_frac(swr: f32) -> f32 {
    (swr.max(1.0).ln() / SWR_MAX.ln()).clamp(0.0, 1.0)
}

/// Which face the meter wears. Cycled by clicking the meter; persisted in the
/// client's view state.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SmeterStyle {
    /// Analog moving-coil instrument with a swinging needle.
    #[default]
    Needle,
    /// Horizontal gradient bar with a graduated scale beneath it.
    Bar,
    /// Scrolling trace of the last quarter-minute — reads fading and QSB (and,
    /// on transmit, how SWR behaved across the over) the way neither of the
    /// instantaneous faces can.
    Trace,
}

impl SmeterStyle {
    /// The next style in the click cycle.
    pub fn next(self) -> Self {
        match self {
            SmeterStyle::Needle => SmeterStyle::Bar,
            SmeterStyle::Bar => SmeterStyle::Trace,
            SmeterStyle::Trace => SmeterStyle::Needle,
        }
    }
}

/// Draw the S-meter in the selected style, filling the box's full interior.
/// Returns the (clickable) response so the caller can cycle the style.
pub fn show(ui: &mut Ui, meters: Option<&Meters>, style: SmeterStyle) -> Response {
    // Fill whatever the (zero-padding) box hands us.
    let size = ui.available_size();
    match style {
        SmeterStyle::Needle => show_needle(ui, meters, size),
        SmeterStyle::Bar => show_bar(ui, meters, size),
        SmeterStyle::Trace => show_trace(ui, meters, size),
    }
}

// ---------------------------------------------------------------------------
// Gradient primitives
// ---------------------------------------------------------------------------

/// A colour ramp stop, positioned in scale-fraction space.
type Stop = (f32, Color32);

/// S-meter ramp: cool blues climbing to ice at S9, then the over-S9 half in
/// amber through to red.
const S_STOPS: &[Stop] = &[
    (0.00, Color32::from_rgb(0x16, 0x62, 0x7e)),
    (0.26, Color32::from_rgb(0x1d, 0xa8, 0xcf)),
    (0.4737, Color32::from_rgb(0x8e, 0xec, 0xff)),
    (0.4738, Color32::from_rgb(0xff, 0xd2, 0x3f)),
    (0.74, Color32::from_rgb(0xff, 0x8a, 0x3f)),
    (1.00, Color32::from_rgb(0xff, 0x2a, 0x55)),
];

/// SWR ramp: green while the match is good, amber as it approaches 3:1, and
/// the whole upper half — everything past 3:1 — in red.
const SWR_STOPS: &[Stop] = &[
    (0.00, Color32::from_rgb(0x3f, 0xe0, 0x86)),
    (0.32, Color32::from_rgb(0x9d, 0xe0, 0x5c)),
    (0.4999, Color32::from_rgb(0xff, 0xc0, 0x3a)),
    (0.50, Color32::from_rgb(0xff, 0x3c, 0x46)),
    (1.00, Color32::from_rgb(0xd6, 0x0c, 0x2e)),
];

/// Drive/ALC ramp, used when the rig reports no SWR bridge.
const ALC_STOPS: &[Stop] = &[
    (0.00, Color32::from_rgb(0x1d, 0xa8, 0xcf)),
    (0.70, Color32::from_rgb(0x8e, 0xec, 0xff)),
    (0.86, Color32::from_rgb(0xff, 0xd2, 0x3f)),
    (1.00, Color32::from_rgb(0xff, 0x2a, 0x55)),
];

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let c = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_premultiplied(
        c(a.r(), b.r()),
        c(a.g(), b.g()),
        c(a.b(), b.b()),
        c(a.a(), b.a()),
    )
}

/// Sample a stop list at `f`.
fn ramp(stops: &[Stop], f: f32) -> Color32 {
    let f = f.clamp(0.0, 1.0);
    let mut i = 0;
    while i + 2 < stops.len() && f > stops[i + 1].0 {
        i += 1;
    }
    let (a, ca) = stops[i];
    let (b, cb) = stops[i + 1];
    mix(ca, cb, if b > a { (f - a) / (b - a) } else { 0.0 })
}

fn push_quad(mesh: &mut Mesh, a: Pos2, b: Pos2, ca: Color32, cb: Color32) {
    let base = mesh.vertices.len() as u32;
    mesh.vertices.push(Vertex { pos: a, uv: WHITE_UV, color: ca });
    mesh.vertices.push(Vertex { pos: b, uv: WHITE_UV, color: cb });
    if base >= 2 {
        mesh.indices.extend_from_slice(&[base - 2, base - 1, base, base - 1, base + 1, base]);
    }
}

/// Vertical two-stop gradient over `rect`.
fn grad_v(p: &Painter, rect: Rect, top: Color32, bottom: Color32) {
    let mut mesh = Mesh::default();
    push_quad(&mut mesh, rect.left_top(), rect.left_bottom(), top, bottom);
    push_quad(&mut mesh, rect.right_top(), rect.right_bottom(), top, bottom);
    p.add(Shape::mesh(mesh));
}

/// Horizontal gradient over `rect`, taking its colour from `col` sampled across
/// the scale fractions `f0..f1`. The fill is shaded top-to-bottom as well, so a
/// bar reads as a lit tube rather than a flat block.
fn grad_h(p: &Painter, rect: Rect, f0: f32, f1: f32, col: impl Fn(f32) -> Color32) {
    if rect.width() <= 0.0 {
        return;
    }
    let segs = (rect.width() / 3.0).ceil().clamp(1.0, 256.0) as usize;
    let mut top = Mesh::default();
    let mut bot = Mesh::default();
    let mid = rect.top() + rect.height() * 0.42;
    for i in 0..=segs {
        let t = i as f32 / segs as f32;
        let x = rect.left() + rect.width() * t;
        let c = col(f0 + (f1 - f0) * t);
        push_quad(&mut top, pos2(x, rect.top()), pos2(x, mid), shade(c, 0.42), c);
        push_quad(&mut bot, pos2(x, mid), pos2(x, rect.bottom()), c, shade(c, -0.42));
    }
    p.add(Shape::mesh(top));
    p.add(Shape::mesh(bot));
}

/// Lighten (`amount` > 0) or darken a colour, keeping its alpha.
fn shade(c: Color32, amount: f32) -> Color32 {
    if amount >= 0.0 {
        mix(c, Color32::from_rgba_premultiplied(c.a(), c.a(), c.a(), c.a()), amount)
    } else {
        mix(c, Color32::from_rgba_premultiplied(0, 0, 0, c.a()), -amount)
    }
}

/// Soft radial bloom, fading to nothing at `radius`.
fn glow(p: &Painter, center: Pos2, radius: f32, color: Color32) {
    const N: u32 = 40;
    let mut mesh = Mesh::default();
    mesh.vertices.push(Vertex { pos: center, uv: WHITE_UV, color });
    for i in 0..=N {
        let a = i as f32 / N as f32 * std::f32::consts::TAU;
        mesh.vertices.push(Vertex {
            pos: center + vec2(radius * a.cos(), radius * a.sin()),
            uv: WHITE_UV,
            color: Color32::TRANSPARENT,
        });
    }
    for i in 1..=N {
        mesh.indices.extend_from_slice(&[0, i, i + 1]);
    }
    p.add(Shape::mesh(mesh));
}

/// The analog scale's geometry: a pivot far below the box, a rail radius, and
/// the half-angle the needle sweeps either side of vertical.
struct Arc {
    pivot: Pos2,
    rad: f32,
    half: f32,
}

impl Arc {
    /// Needle angle for a scale fraction, 0 at the left stop … 1 at the right.
    fn ang(&self, f: f32) -> f32 {
        (f.clamp(0.0, 1.0) - 0.5) * 2.0 * self.half
    }

    /// The point `r` out from the pivot along `a`.
    fn at(&self, r: f32, a: f32) -> Pos2 {
        self.pivot + vec2(r * a.sin(), -r * a.cos())
    }

    /// How many segments a sweep of `f0..f1` needs to look smooth.
    fn segs(&self, f0: f32, f1: f32, per_turn: f32) -> usize {
        ((f1 - f0) * per_turn).ceil().clamp(1.0, 256.0) as usize
    }
}

/// A bloom hugging the rail: brightest on the arc itself and fading to nothing
/// `width` either side of it, so the lit part of the scale looks like it is
/// glowing rather than sitting inside a rectangle.
fn arc_glow(p: &Painter, arc: &Arc, width: f32, f0: f32, f1: f32, col: impl Fn(f32) -> Color32) {
    if f1 <= f0 {
        return;
    }
    let segs = arc.segs(f0, f1, 96.0);
    let (mut inner, mut outer) = (Mesh::default(), Mesh::default());
    for i in 0..=segs {
        let f = f0 + (f1 - f0) * i as f32 / segs as f32;
        let a = arc.ang(f);
        let color = col(f);
        let (lo, mid, hi) =
            (arc.at(arc.rad - width, a), arc.at(arc.rad, a), arc.at(arc.rad + width, a));
        push_quad(&mut inner, lo, mid, Color32::TRANSPARENT, color);
        push_quad(&mut outer, mid, hi, color, Color32::TRANSPARENT);
    }
    p.add(Shape::mesh(inner));
    p.add(Shape::mesh(outer));
}

/// A band swept between two radii over the scale fractions `f0..f1`, coloured
/// by `col`. This is the analog scale's rail — one mesh, so the ramp runs
/// smoothly along the arc instead of stepping between segments.
fn arc_band(
    p: &Painter,
    arc: &Arc,
    radii: (f32, f32),
    f0: f32,
    f1: f32,
    col: impl Fn(f32) -> Color32,
) {
    if f1 <= f0 {
        return;
    }
    let segs = arc.segs(f0, f1, 128.0);
    let mut mesh = Mesh::default();
    for i in 0..=segs {
        let f = f0 + (f1 - f0) * i as f32 / segs as f32;
        let a = arc.ang(f);
        let color = col(f);
        push_quad(&mut mesh, arc.at(radii.0, a), arc.at(radii.1, a), color, color);
    }
    p.add(Shape::mesh(mesh));
}

// ---------------------------------------------------------------------------
// Scales
// ---------------------------------------------------------------------------

/// One graduation: where it sits, how prominent it is, what it is called, and
/// whether it falls in the scale's hot (over-limit) zone.
struct Tick {
    f: f32,
    major: bool,
    label: Option<String>,
    hot: bool,
}

/// A reference level for the trace view's gridlines. Deliberately sparser than
/// the graduations: a 72 px plot only has room for a handful.
struct Grid {
    f: f32,
    label: String,
    hot: bool,
}

/// Which quantity a scale measures. The trace view keys its history on this, so
/// switching between receive and transmit starts a fresh plot rather than
/// splicing S-units onto SWR.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ScaleKind {
    S,
    Swr,
    Alc,
}

/// A meter scale: its graduations, its gridlines, and the ramp along its rail.
struct Scale {
    kind: ScaleKind,
    ticks: Vec<Tick>,
    grid: Vec<Grid>,
    stops: &'static [Stop],
}

fn grid_at(f: f32, label: &str, hot: bool) -> Grid {
    Grid { f, label: label.to_string(), hot }
}

/// S0…S9 every S-unit (numbered on the odd ones), then 10 dB steps to S9+60.
fn s_scale() -> Scale {
    let mut ticks = Vec::new();
    for i in 0..=9 {
        let major = i % 2 == 1;
        ticks.push(Tick {
            f: frac_of(S9_DBM - (9 - i) as f32 * 6.0),
            major,
            label: major.then(|| i.to_string()),
            hot: false,
        });
    }
    for over in [10.0f32, 20.0, 30.0, 40.0, 50.0, 60.0] {
        let major = (over as i32) % 20 == 0;
        ticks.push(Tick {
            f: frac_of(S9_DBM + over),
            major,
            label: major.then(|| format!("+{over:.0}")),
            hot: true,
        });
    }
    let grid = vec![
        grid_at(frac_of(S9_DBM - 36.0), "3", false),
        grid_at(frac_of(S9_DBM - 18.0), "6", false),
        grid_at(frac_of(S9_DBM), "9", false),
        grid_at(frac_of(S9_DBM + 30.0), "+30", true),
    ];
    Scale { kind: ScaleKind::S, ticks, grid, stops: S_STOPS }
}

/// 1:1 … 9:1, logarithmic, with 3:1 at mid-travel and everything past it hot.
fn swr_scale() -> Scale {
    const GRADS: &[(f32, bool)] = &[
        (1.0, true),
        (1.2, false),
        (1.5, true),
        (2.0, true),
        (2.5, false),
        (3.0, true),
        (4.0, false),
        (5.0, true),
        (7.0, false),
        (9.0, true),
    ];
    let mut ticks = Vec::new();
    for &(swr, major) in GRADS {
        // Whole ratios read better without a trailing zero; 1.5 needs its .5.
        let label = if swr.fract() == 0.0 { format!("{swr:.0}") } else { format!("{swr:.1}") };
        ticks.push(Tick {
            f: swr_frac(swr),
            major,
            label: major.then_some(label),
            hot: swr >= 3.0,
        });
    }
    let grid = vec![
        grid_at(swr_frac(1.5), "1.5", false),
        grid_at(swr_frac(2.0), "2", false),
        grid_at(swr_frac(3.0), "3", true),
        grid_at(swr_frac(5.0), "5", true),
    ];
    Scale { kind: ScaleKind::Swr, ticks, grid, stops: SWR_STOPS }
}

/// 0…100 % drive, the fallback when the rig has no SWR bridge.
fn alc_scale() -> Scale {
    let ticks = (0..=10)
        .map(|i| {
            let f = i as f32 / 10.0;
            let major = i % 5 == 0;
            Tick { f, major, label: major.then(|| format!("{}", i * 10)), hot: f >= 0.9 }
        })
        .collect();
    let grid = vec![
        grid_at(0.25, "25", false),
        grid_at(0.5, "50", false),
        grid_at(0.75, "75", false),
        grid_at(0.95, "95", true),
    ];
    Scale { kind: ScaleKind::Alc, ticks, grid, stops: ALC_STOPS }
}

/// What the instrument is currently displaying: the scale to draw, where the
/// needle sits on it, and the two readouts in the header.
struct Reading {
    scale: Scale,
    frac: f32,
    /// Headline value in the top-left chip, and the chip's accent.
    chip: String,
    accent: Color32,
    /// Right-hand reading.
    right: String,
    /// Whether `right` is a co-equal reading (TX) or a quiet sub-label (RX).
    right_strong: bool,
}

fn reading(meters: Option<&Meters>) -> Reading {
    if let Some(tx) = meters.and_then(|m| m.tx.as_ref()) {
        let power = match tx.fwd_w {
            Some(w) => format!("{w:.1} W"),
            None => format!("ALC {:.0}%", tx.alc.clamp(0.0, 1.0) * 100.0),
        };
        return match tx.swr {
            Some(swr) => Reading {
                scale: swr_scale(),
                frac: swr_frac(swr),
                chip: format!("SWR {swr:.1}"),
                accent: RED,
                right: power,
                right_strong: true,
            },
            // No SWR bridge: the needle falls back to the engine's own drive
            // level, which is the only TX quantity every backend reports.
            None => Reading {
                scale: alc_scale(),
                frac: tx.alc.clamp(0.0, 1.0),
                chip: "TX".to_string(),
                accent: RED,
                right: power,
                right_strong: true,
            },
        };
    }

    let dbm = meters.map(|m| m.s_dbm).unwrap_or(DBM_LO);
    let (primary, secondary) = match meters {
        None => ("—".to_string(), String::new()),
        Some(_) if dbm > S9_DBM => (format!("S9+{:.0}", dbm - S9_DBM), format!("{dbm:.0} dBm")),
        Some(m) => (format!("S{}", m.s_units().0), format!("{dbm:.0} dBm")),
    };
    Reading {
        scale: s_scale(),
        frac: frac_of(dbm),
        chip: primary,
        accent: CYAN,
        right: secondary,
        right_strong: false,
    }
}

// ---------------------------------------------------------------------------
// Shared chrome
// ---------------------------------------------------------------------------

/// Paint the instrument face over the full box and return the rect + response.
/// The backlight goes red on transmit — a cue that costs no space in a box this
/// small, and that reads before any of the text does.
fn face(ui: &mut Ui, size: Vec2, tx: bool) -> (Rect, Response) {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let p = ui.painter_at(rect);
    grad_v(&p, rect, FACE_TOP, FACE_BOT);
    // Backlight bloom rising from behind the scale, and a hairline of specular
    // along the top edge so the glass reads as glass.
    glow(
        &p,
        pos2(rect.center().x, rect.top() + rect.height() * 0.72),
        rect.width() * 0.52,
        if tx { BACKLIGHT_TX.linear_multiply(0.34) } else { BACKLIGHT.linear_multiply(0.30) },
    );
    p.hline(
        rect.x_range(),
        rect.top() + 0.5,
        Stroke::new(1.0, Color32::from_rgb(0x2c, 0x44, 0x60).linear_multiply(0.7)),
    );
    (rect, resp)
}

/// Draw the box border on top of the finished meter (the meter face already
/// covers the whole rect, so the frame's own border would be hidden).
fn border(ui: &Ui, rect: Rect) {
    ui.painter_at(rect).rect_stroke(
        rect,
        0.0,
        Stroke::new(1.0, crate::theme::LINE_LIT),
        StrokeKind::Inside,
    );
}

/// A small tag: gradient-filled, accent-outlined, with the text inside. Returns
/// its rect so the caller can lay the next element out beside it.
fn chip(p: &Painter, top_left: Pos2, text: &str, accent: Color32, pt: f32) -> Rect {
    let g = p.layout_no_wrap(text.to_owned(), FontId::monospace(pt), Color32::WHITE);
    let pad = vec2(5.0, 1.5);
    let rect = Rect::from_min_size(top_left, g.size() + pad * 2.0);
    grad_v(p, rect, accent.linear_multiply(0.40), accent.linear_multiply(0.14));
    p.rect_stroke(rect, 0.0, Stroke::new(1.0, accent.linear_multiply(0.80)), StrokeKind::Inside);
    p.galley(rect.min + pad, g, mix(accent, Color32::WHITE, 0.65));
    rect
}

/// The top row every face shares: the headline value in a chip on the left, the
/// secondary reading on the right. Returns the strip it occupies.
fn header(p: &Painter, rect: Rect, r: &Reading, k: f32) -> Rect {
    let head_y = rect.top() + 3.0 * k;
    let chip_rect = chip(p, pos2(rect.left() + 6.0 * k, head_y), &r.chip, r.accent, 11.0 * k);
    if !r.right.is_empty() {
        let (pt, ink) = if r.right_strong { (14.0 * k, READOUT) } else { (11.0 * k, SUBDUED) };
        p.text(
            pos2(rect.right() - 6.0 * k, head_y),
            Align2::RIGHT_TOP,
            &r.right,
            FontId::monospace(pt),
            ink,
        );
    }
    Rect::from_min_max(rect.left_top(), pos2(rect.right(), chip_rect.bottom() + 2.0 * k))
}

/// Peak-hold state: the highest fraction seen, and when it was set.
#[derive(Clone, Copy)]
struct Peak {
    frac: f32,
    at: f64,
}

/// Needle motion: a lightly-damped second-order follower. `vel` is in fractions
/// of full scale per second.
#[derive(Clone, Copy)]
struct Motion {
    frac: f32,
    vel: f32,
}

/// Step the needle towards `target` and return where it is now, so it swings
/// into a new reading and settles instead of teleporting. Asks for a repaint
/// while it is still moving, so the swing stays smooth even when the spectrum
/// — and with it the window's frame rate — is running slowly.
fn needle_motion(ui: &Ui, id: Id, target: f32) -> f32 {
    /// How hard the needle is pulled towards the reading.
    const OMEGA: f32 = 26.0;
    /// Damping ratio. Just under 1: the hint of overshoot is what makes a
    /// moving-coil movement look like one.
    const ZETA: f32 = 0.75;

    let dt = ui.input(|i| i.stable_dt).clamp(0.001, 0.05);
    let mut m = ui.data(|d| d.get_temp::<Motion>(id)).unwrap_or(Motion { frac: target, vel: 0.0 });
    // Semi-implicit Euler — stable at the step sizes a UI frame hands us.
    m.vel += (OMEGA * OMEGA * (target - m.frac) - 2.0 * ZETA * OMEGA * m.vel) * dt;
    let next = m.frac + m.vel * dt;
    if !(0.0..=1.0).contains(&next) {
        m.vel = 0.0; // the movement has hit its end stop
    }
    m.frac = next.clamp(0.0, 1.0);
    ui.data_mut(|d| d.insert_temp(id, m));
    if (m.frac - target).abs() > 0.0005 || m.vel.abs() > 0.002 {
        animate(ui);
    }
    m.frac
}

/// Ask for the next frame at the meter's own rate. Deliberately *not* a bare
/// `request_repaint`: on a busy band the needle is always creeping somewhere,
/// and an unpaced request would pin the whole window — panadapter included — at
/// the display's refresh rate for the sake of one small widget.
fn animate(ui: &Ui) {
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
}

/// The decaying peak marker for `frac` — held briefly at the maximum, then
/// falling back until the live reading catches up. `None` once it has.
fn peak_hold(ui: &Ui, id: Id, frac: f32) -> Option<f32> {
    /// Seconds the marker sits still before it starts falling.
    const HOLD: f64 = 0.7;
    /// How fast it falls afterwards, in fractions of full scale per second.
    const FALL: f32 = 0.55;

    let now = ui.input(|i| i.time);
    let shown = |p: Peak| p.frac - FALL * (now - p.at - HOLD).max(0.0) as f32;
    let prev = ui.data(|d| d.get_temp::<Peak>(id));
    let peak = match prev {
        Some(p) if shown(p) > frac => p,
        _ => Peak { frac, at: now },
    };
    ui.data_mut(|d| d.insert_temp(id, peak));
    let showing = shown(peak) > frac + 0.01;
    if showing {
        animate(ui); // keep the fall-back smooth even if no new reading arrives
    }
    showing.then(|| shown(peak).clamp(0.0, 1.0))
}

// ---------------------------------------------------------------------------
// Bar style
// ---------------------------------------------------------------------------

/// One labelled bar row: a recessed trough, a gradient fill with a lit leading
/// edge, and the peak marker riding on top.
fn bar_row(p: &Painter, rect: Rect, scale: &Scale, frac: f32, peak: Option<f32>) {
    grad_v(p, rect, Color32::from_rgb(0x04, 0x07, 0x0d), Color32::from_rgb(0x13, 0x1f, 0x2f));
    p.rect_stroke(
        rect,
        0.0,
        Stroke::new(1.0, Color32::from_rgb(0x1e, 0x2e, 0x42)),
        StrokeKind::Inside,
    );
    let inner = rect.shrink(1.0);
    // The unlit remainder of the rail keeps the ramp's hue, so the red zone is
    // visible before the bar ever reaches it.
    grad_h(p, inner, 0.0, 1.0, |f| ramp(scale.stops, f).linear_multiply(0.16));

    let x = inner.left() + inner.width() * frac.clamp(0.0, 1.0);
    if x > inner.left() {
        let fill = Rect::from_min_max(inner.min, pos2(x, inner.bottom()));
        grad_h(p, fill, 0.0, frac, |f| ramp(scale.stops, f));
        // Lit leading edge: a bar this small reads much faster with one.
        p.vline(
            x - 0.5,
            Rangef::new(inner.top(), inner.bottom()),
            Stroke::new(1.5, shade(ramp(scale.stops, frac), 0.65)),
        );
    }
    if let Some(pk) = peak {
        let px = inner.left() + inner.width() * pk;
        p.vline(
            px,
            Rangef::new(inner.top(), inner.bottom()),
            Stroke::new(1.5, shade(ramp(scale.stops, pk), 0.55)),
        );
    }
}

/// Graduations under a bar row: ticks against the bar's bottom edge and the
/// numbers below them, nudged inwards at the ends so the outermost number is
/// never half-eaten by the frame.
fn bar_ticks(p: &Painter, bar: Rect, bounds: Rect, scale: &Scale, k: f32, num_top: f32) {
    for t in &scale.ticks {
        let x = (bar.left() + bar.width() * t.f).round();
        let len = if t.major { 5.0 * k } else { 3.0 * k };
        let hot = usize::from(t.hot);
        p.vline(
            x,
            Rangef::new(bar.bottom() + 2.0 * k, bar.bottom() + 2.0 * k + len),
            Stroke::new(1.0, if t.major { TICK_MAJOR[hot] } else { TICK_MINOR[hot] }),
        );
        if let Some(label) = &t.label {
            let g = p.layout_no_wrap(label.clone(), FontId::monospace(9.0 * k), LABEL[hot]);
            let x = (x - g.size().x * 0.5)
                .clamp(bounds.left() + 1.0, bounds.right() - g.size().x - 1.0);
            p.galley(pos2(x, num_top), g, LABEL[hot]);
        }
    }
}

fn show_bar(ui: &mut Ui, meters: Option<&Meters>, size: Vec2) -> Response {
    let tx = meters.and_then(|m| m.tx);
    let (rect, resp) = face(ui, size, tx.is_some());
    let p = ui.painter_at(rect);
    let k = (rect.height() / 72.0).clamp(0.55, 2.0);
    let r = reading(meters);
    let peak = peak_hold(ui, resp.id, r.frac);

    let left = rect.left() + 6.0 * k;
    let right = rect.right() - 6.0 * k;
    header(&p, rect, &r, k);

    if let Some(tx) = tx {
        // Two stacked rows: the engine's drive on top, SWR below on its own
        // logarithmic scale. Without an SWR bridge only the drive row is drawn,
        // grown to fill the space the SWR row would have taken.
        let alc = alc_scale();
        let row = |top: f32, bottom: f32| {
            Rect::from_min_max(
                pos2(left + 26.0 * k, rect.top() + top * k),
                pos2(right, rect.top() + bottom * k),
            )
        };
        let (drive_rect, swr_rect) = match tx.swr {
            Some(_) => (row(20.0, 33.0), Some(row(36.0, 49.0))),
            None => (row(24.0, 42.0), None),
        };
        let row_label = |rect: Rect, text: &str| {
            p.text(
                pos2(rect.left() - 4.0 * k, rect.center().y),
                Align2::RIGHT_CENTER,
                text,
                FontId::monospace(9.5 * k),
                SUBDUED,
            );
        };
        row_label(drive_rect, "ALC");
        bar_row(&p, drive_rect, &alc, tx.alc.clamp(0.0, 1.0), None);
        match swr_rect {
            Some(swr_rect) => {
                row_label(swr_rect, "SWR");
                bar_row(&p, swr_rect, &r.scale, r.frac, peak);
                bar_ticks(&p, swr_rect, rect, &r.scale, k, swr_rect.bottom() + 8.0 * k);
            }
            None => bar_ticks(&p, drive_rect, rect, &alc, k, drive_rect.bottom() + 8.0 * k),
        }
        border(ui, rect);
        return resp;
    }

    let bar_rect =
        Rect::from_min_max(pos2(left, rect.top() + 21.0 * k), pos2(right, rect.top() + 45.0 * k));
    bar_row(&p, bar_rect, &r.scale, r.frac, peak);
    bar_ticks(&p, bar_rect, rect, &r.scale, k, bar_rect.bottom() + 8.0 * k);
    border(ui, rect);
    resp
}

// ---------------------------------------------------------------------------
// Analog needle style
// ---------------------------------------------------------------------------

/// The needle: a tapered blade with a drop shadow and a soft bloom, running
/// from the (off-box) pivot out to the scale rail.
fn draw_needle(p: &Painter, pivot: Pos2, a: f32, r_tip: f32, k: f32) {
    let dir = vec2(a.sin(), -a.cos());
    let nrm = vec2(-dir.y, dir.x);
    // Width and colour along the blade. Only the outer third is ever on-box, so
    // the taper is tuned to look right there.
    const PROFILE: &[(f32, f32)] = &[(0.0, 1.0), (0.6, 0.82), (0.9, 0.48), (1.0, 0.05)];
    let ink = |t: f32| {
        if t < 0.9 {
            mix(Color32::from_rgb(0x9e, 0x0c, 0x22), Color32::from_rgb(0xff, 0x2c, 0x38), t / 0.9)
        } else {
            mix(
                Color32::from_rgb(0xff, 0x2c, 0x38),
                Color32::from_rgb(0xff, 0xe2, 0xe6),
                (t - 0.9) / 0.1,
            )
        }
    };

    // `soft` fades the blade out sideways, which is what makes the bloom read
    // as light rather than as a second, wider needle.
    let blade = |width: f32, off: Vec2, soft: bool, col: &dyn Fn(f32) -> Color32| {
        let (mut left, mut right) = (Mesh::default(), Mesh::default());
        for &(t, w) in PROFILE {
            let c = pivot + dir * (r_tip * t) + off;
            let hw = nrm * (width * w);
            let (core, edge) = (col(t), if soft { Color32::TRANSPARENT } else { col(t) });
            push_quad(&mut left, c - hw, c, edge, core);
            push_quad(&mut right, c, c + hw, core, edge);
        }
        p.add(Shape::mesh(left));
        p.add(Shape::mesh(right));
    };

    blade(7.0 * k, Vec2::ZERO, true, &|_| RED.linear_multiply(0.22));
    blade(3.6 * k, vec2(1.5, 2.0), false, &|_| Color32::from_black_alpha(150));
    blade(3.2 * k, Vec2::ZERO, false, &ink);
}

fn show_needle(ui: &mut Ui, meters: Option<&Meters>, size: Vec2) -> Response {
    let tx = meters.is_some_and(|m| m.tx.is_some());
    let (rect, resp) = face(ui, size, tx);
    let p = ui.painter_at(rect);
    let k = (rect.height() / 72.0).clamp(0.55, 2.0);
    let r = reading(meters);
    let peak = peak_hold(ui, resp.id, r.frac);
    // The rail's lit length follows the needle rather than the raw reading, so
    // the two never disagree mid-swing.
    let frac = needle_motion(ui, resp.id, r.frac);

    // A wide, shallow arc: the pivot sits far below the short box, so the
    // needle sweeps a broad band across it and the scale's ends dip towards the
    // lower corners — leaving the top corners free for the readouts. The sweep
    // is what sets the radius: the chord has to span the box.
    let half = 31.0_f32.to_radians();
    let rad = ((rect.width() - 14.0) / (2.0 * half.sin())).max(24.0);
    let arc = Arc { pivot: pos2(rect.center().x, rect.top() + 13.0 * k + rad), rad, half };
    let band = 2.6 * k;
    let rail = (rad - band, rad + band);
    let col = |f: f32| ramp(r.scale.stops, f);

    // Rail: the whole span dimmed, the travelled part lit, with a bloom under
    // the lit part so the instrument looks illuminated rather than printed.
    arc_band(&p, &arc, rail, 0.0, 1.0, |f| col(f).linear_multiply(0.18));
    arc_glow(&p, &arc, band * 3.2, 0.0, frac, |f| col(f).linear_multiply(0.22));
    arc_band(&p, &arc, rail, 0.0, frac, col);

    // Graduations, inside the rail.
    let tick_out = rad - band - 1.5 * k;
    for t in &r.scale.ticks {
        let a = arc.ang(t.f);
        let hot = usize::from(t.hot);
        let (len, w, ink) =
            if t.major { (6.0 * k, 1.4, TICK_MAJOR[hot]) } else { (3.5 * k, 1.0, TICK_MINOR[hot]) };
        p.line_segment([arc.at(tick_out - len, a), arc.at(tick_out, a)], Stroke::new(w, ink));
        if let Some(label) = &t.label {
            p.text(
                arc.at(rad - 17.5 * k, a),
                Align2::CENTER_CENTER,
                label,
                FontId::monospace(9.0 * k),
                LABEL[hot],
            );
        }
    }

    if let Some(pk) = peak {
        let a = arc.ang(pk);
        p.line_segment(
            [arc.at(rail.0, a), arc.at(rad + band * 2.0, a)],
            Stroke::new(1.6, shade(col(pk), 0.6)),
        );
    }

    draw_needle(&p, arc.pivot, arc.ang(frac), rad - band * 1.6, k);

    // Readouts along the top edge, where the arc has dipped out of the way.
    header(&p, rect, &r, k);
    border(ui, rect);
    resp
}

// ---------------------------------------------------------------------------
// Trace style
// ---------------------------------------------------------------------------

/// Samples the trace keeps and the rate it takes them at: 240 at 16 Hz is a
/// fifteen-second window, and roughly one sample per pixel at the meter's size.
const TRACE_LEN: usize = 240;
const TRACE_HZ: f64 = 16.0;

/// The rolling plot behind the trace face.
#[derive(Clone)]
struct Trace {
    vals: Vec<f32>,
    /// What the samples measure; a change starts a fresh plot.
    kind: ScaleKind,
    /// Wall-clock time the next sample is due.
    next: f64,
}

/// Append to the rolling history and hand back the whole window.
fn trace_history(ui: &Ui, id: Id, frac: f32, kind: ScaleKind) -> Vec<f32> {
    let now = ui.input(|i| i.time);
    animate(ui); // the plot scrolls on the clock, with or without fresh readings
    ui.data_mut(|d| {
        let fresh = || Trace { vals: vec![frac; TRACE_LEN], kind, next: now };
        let t = d.get_temp_mut_or_insert_with(id, fresh);
        // Receive ↔ transmit changes what is being plotted; start over rather
        // than splice two different quantities into one line.
        if t.kind != kind {
            *t = fresh();
        }
        // Advance in whole steps so the scroll follows the clock rather than the
        // frame rate. A long stall slides the window along instead of squashing
        // it — and the step cap stops a huge jump spinning here.
        let mut steps = 0;
        while now >= t.next && steps < TRACE_LEN {
            t.vals.rotate_left(1);
            t.vals[TRACE_LEN - 1] = frac;
            t.next += 1.0 / TRACE_HZ;
            steps += 1;
        }
        if steps == TRACE_LEN {
            t.next = now + 1.0 / TRACE_HZ;
        }
        t.vals.clone()
    })
}

fn show_trace(ui: &mut Ui, meters: Option<&Meters>, size: Vec2) -> Response {
    let tx = meters.is_some_and(|m| m.tx.is_some());
    let (rect, resp) = face(ui, size, tx);
    let p = ui.painter_at(rect);
    let k = (rect.height() / 72.0).clamp(0.55, 2.0);
    let r = reading(meters);
    let hist = trace_history(ui, resp.id, r.frac, r.scale.kind);
    let col = |f: f32| ramp(r.scale.stops, f);

    // A gutter on the right carries the level axis, so the newest sample sits
    // right up against the scale it is read on.
    let plot = Rect::from_min_max(
        pos2(rect.left() + 1.0, rect.top() + 1.0),
        pos2(rect.right() - 24.0 * k, rect.bottom() - 1.0),
    );
    let y_of = |f: f32| plot.bottom() - plot.height() * f.clamp(0.0, 1.0);
    // Where the header ends; nothing that would be drawn behind it is drawn.
    let head_bottom = rect.top() + 20.0 * k;

    // Time gridlines every five seconds, anchored to "now" at the right edge.
    let span_s = TRACE_LEN as f32 / TRACE_HZ as f32;
    let mut secs = 5.0;
    while secs < span_s {
        let x = plot.right() - plot.width() * secs / span_s;
        p.vline(x, Rangef::new(plot.top(), plot.bottom()), Stroke::new(1.0, GRID_LINE[0]));
        secs += 5.0;
    }

    // Level gridlines, labelled out in the gutter.
    for g in &r.scale.grid {
        let y = y_of(g.f);
        if y < head_bottom {
            continue;
        }
        let hot = usize::from(g.hot);
        p.hline(Rangef::new(plot.left(), plot.right()), y, Stroke::new(1.0, GRID_LINE[hot]));
        p.text(
            pos2(plot.right() + 3.0 * k, y),
            Align2::LEFT_CENTER,
            &g.label,
            FontId::monospace(8.5 * k),
            GRID_LABEL[hot],
        );
    }

    // The trace itself: one column per sample, each carrying the colour of its
    // own level, so the plot reads as a band of signal rather than a silhouette.
    let mut fill = Mesh::default();
    // Meshes are not antialiased, so the fill's top edge would show a one-pixel
    // staircase. `feather` fades the last pixel of it out and smooths it away.
    let mut feather = Mesh::default();
    let mut ridge = Vec::with_capacity(TRACE_LEN);
    for (i, &v) in hist.iter().enumerate() {
        let x = plot.left() + plot.width() * i as f32 / (TRACE_LEN - 1) as f32;
        let y = y_of(v);
        let c = col(v);
        push_quad(
            &mut fill,
            pos2(x, y),
            pos2(x, plot.bottom()),
            c.linear_multiply(0.60),
            c.linear_multiply(0.05),
        );
        push_quad(
            &mut feather,
            pos2(x, y - 1.2),
            pos2(x, y),
            Color32::TRANSPARENT,
            c.linear_multiply(0.60),
        );
        ridge.push(pos2(x, y));
    }
    p.add(Shape::mesh(fill));
    p.add(Shape::mesh(feather));
    p.add(Shape::line(ridge, Stroke::new(1.5, READOUT.linear_multiply(0.75))));

    // "Now": a hairline across the plot at the live level and a lit cap at the
    // axis, so the current reading is findable without tracing the line.
    let y = y_of(r.frac);
    p.hline(
        Rangef::new(plot.left(), plot.right()),
        y,
        Stroke::new(1.0, col(r.frac).linear_multiply(0.30)),
    );
    glow(&p, pos2(plot.right() - 1.0, y), 9.0 * k, col(r.frac).linear_multiply(0.35));
    p.rect_filled(
        Rect::from_min_max(
            pos2(plot.right() - 4.0 * k, y - 1.5 * k),
            pos2(plot.right(), y + 1.5 * k),
        ),
        0.0,
        shade(col(r.frac), 0.4),
    );

    // The trace runs full-bleed, so the header needs a scrim under it.
    grad_v(
        &p,
        Rect::from_min_max(rect.left_top(), pos2(rect.right(), head_bottom)),
        Color32::from_black_alpha(180),
        Color32::TRANSPARENT,
    );
    header(&p, rect, &r, k);
    border(ui, rect);
    resp
}
