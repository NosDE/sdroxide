//! The panadapter: spectrum line + frequency scale + GPU waterfall, with
//! drag-pan, wheel-zoom (around the cursor), click-to-tune (shift-click places
//! the second receiver — the sub when it is running, VFO B otherwise),
//! draggable passband filter edges for both receivers, a sub-receiver that
//! tunes by dragging inside its own passband, and a shift+drag bandwidth
//! measurement ruler.

use std::sync::Arc;

use eframe::egui::{
    self, Align2, Color32, CursorIcon, FontId, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Ui,
    pos2, vec2,
};
use sdroxide_types::{
    Command, Mode, RadioState, RxId, SkimmerKind, SkimmerSpot, SpectrumFrame, Spot, Vfo,
    WheelAction, WheelSettings,
};

use crate::view::ViewState;
use crate::waterfall_gpu::WaterfallCallback;

const SCALE_H: f32 = 18.0;
/// Tuning rounds to this step on click-tune, unless the operator has changed
/// [`WheelSettings::click_tune_step_hz`].
const CLICK_TUNE_STEP: f64 = 10.0;
/// Smooth-scroll points per wheel detent, matching the frequency readout so the
/// two feel the same under the finger.
const SCROLL_PER_DETENT: f32 = 30.0;
/// Pixel distance for grabbing a filter edge.
const EDGE_GRAB_PX: f32 = 6.0;
/// Pixel distance for grabbing the sub receiver's tuning line — its passband is
/// grabbable in full, but in SSB the line sits outside it.
const MARKER_GRAB_PX: f32 = 5.0;
/// Constant bearing friction a coasting dial loses speed to, in screen points
/// per second². This sets the character of the flywheel: the coast lasts
/// roughly the release speed divided by this, so a flick twice as fast runs
/// about twice as long — which is how a weighted VFO knob behaves and what
/// makes the gesture predictable.
const FLING_FRICTION: f32 = 1400.0;
/// Viscous drag, per second, on top of the friction. Stops a very hard flick
/// from crossing half a band without making a gentle one feel sticky: an
/// ordinary flick coasts under a second and about half a panadapter width, a
/// hard one a second and a half and a couple of widths.
const FLING_DRAG: f32 = 0.6;
/// Release speed below which the drag was *placing* the dial rather than
/// spinning it, so it stops exactly where it was let go. Careful tuning must
/// stay exact — this is the whole reason there is a threshold at all.
const FLING_MIN_LAUNCH: f32 = 220.0;
/// Speed at which a coast has run out and the dial is parked.
const FLING_STOP: f32 = 25.0;

/// The speed a coasting dial has left after `dt` seconds of friction and drag.
///
/// Both terms are taken as one step so the decay can never carry the speed
/// through zero and spin the dial back the way it came: a decay larger than the
/// speed itself parks it.
fn fling_decay(vx: f32, dt: f32) -> f32 {
    let decay = vx * FLING_DRAG * dt + vx.signum() * FLING_FRICTION * dt;
    if decay.abs() >= vx.abs() { 0.0 } else { vx - decay }
}

/// A dial still coasting after the drag that spun it let go.
///
/// The velocity is in screen points per second, not Hz: a mechanical dial does
/// not know what band scale the display is at, so the same flick has to coast
/// the same *visible* distance whether the view spans 3 kHz or 3 MHz. The Hz
/// step is derived per frame from the span in force at the time.
#[derive(Clone, Copy)]
struct Fling {
    /// True when the sub receiver is the dial that is turning.
    sub: bool,
    /// Signed pointer velocity in points/second; positive is rightwards.
    vx: f32,
}

/// The sub receiver's colour, on the panadapter and on its control module.
/// Deliberately nothing like the main receiver's red, the amber VFO/RTTY
/// markers, or the cyan the digital modes use: two receivers on one waterfall
/// are only readable if telling them apart never needs a second look.
pub const SUB_COLOR: Color32 = Color32::from_rgb(185, 130, 255);

/// Per-frame display tuning from the app. Both the waterfall advance and the
/// time gridlines are driven by the same wall-clock so they scroll in lockstep
/// (the app converts elapsed time × `rows_per_sec` into `rows_to_write`).
#[derive(Clone, Copy)]
pub struct WfTuning {
    /// Waterfall rows to append this frame (from elapsed wall-clock time).
    pub rows_to_write: u32,
    /// Scroll rate in rows/second — used both to advance the waterfall and to
    /// space the 60-second gridlines, so the line tracks the waterfall exactly.
    pub rows_per_sec: f32,
    /// Wall-clock UTC seconds, for the minute-boundary time labels.
    pub now_unix: f64,
    /// EMA coefficient for the spectrum *line* only (1.0 = no smoothing). The
    /// waterfall uses the raw un-averaged frames, so the line's update speed is
    /// decoupled from the waterfall detail.
    pub spectrum_alpha: f32,
    /// Waterfall colour-palette index (from `UiSettings`).
    pub palette: usize,
    /// Optional vertical gradient `(top, bottom)` filling the spectrum area,
    /// `None` when disabled in the UI settings.
    pub gradient: Option<(Color32, Color32)>,
}

/// Exponentially-smoothed spectrum for the trace line, folded once per new
/// frame. Kept separate from the (un-averaged) frame the waterfall consumes so
/// the "spectrum update speed" setting never blurs the waterfall.
#[derive(Default)]
pub struct SpectrumSmooth {
    bins: Vec<f32>,
    center: f64,
    span: f64,
    last_seq: Option<u32>,
    generation: u32,
    alpha: f32,
}

impl SpectrumSmooth {
    fn update(&mut self, f: &SpectrumFrame, alpha: f32) {
        if (self.alpha - alpha).abs() > 1e-6 {
            self.alpha = alpha;
            self.generation = self.generation.wrapping_add(1);
        }
        if self.last_seq == Some(f.seq) {
            return; // same frame redrawn
        }
        self.last_seq = Some(f.seq);
        let mapping_changed = self.bins.len() != f.bins.len()
            || (self.center - f.center_hz).abs() > f.span_hz * 1e-6
            || (self.span - f.span_hz).abs() > f.span_hz * 1e-6;
        if mapping_changed || alpha >= 0.999 {
            self.center = f.center_hz;
            self.span = f.span_hz;
            self.bins = f.bins.iter().map(|&b| b as f32).collect();
            if mapping_changed {
                self.generation = self.generation.wrapping_add(1);
            }
            return;
        }
        for (s, &b) in self.bins.iter_mut().zip(&f.bins) {
            *s += alpha * (b as f32 - *s);
        }
    }
}

// --- skimmer spot boxes ---------------------------------------------------
const SPOT_BOX_W: f32 = 236.0;
const SPOT_BOX_H: f32 = 19.0;
/// Font size for the box's callsign / message text.
const SPOT_CALL_PT: f32 = 13.0;
const SPOT_MSG_PT: f32 = 12.5;
/// Horizontal padding inside a box.
const SPOT_PAD: f32 = 5.0;
/// Vertical gap between staggered lanes.
const SPOT_LANE_GAP: f32 = 3.0;
/// Gap from the newest edge of the waterfall to the first lane.
const SPOT_TOP_MARGIN: f32 = 4.0;
/// Minimum horizontal gap between two boxes sharing a lane.
const SPOT_H_GAP: f32 = 6.0;
/// Horizontal leader length from the signal to the box's left edge.
const SPOT_LEADER: f32 = 16.0;
/// Cap on stacked lanes; spots that would need a deeper lane are dropped.
const SPOT_MAX_LANES: usize = 6;

/// A laid-out skimmer box: its screen rect, the x of the signal it belongs to
/// (the box sits to its right, joined by a leader), and the spot's index.
struct SpotBox {
    rect: Rect,
    sig_x: f32,
    idx: usize,
}

/// Border/leader colour for a spot: cyan when hovered, dim-cyan once a
/// callsign is known, grey otherwise.
fn spot_color(spot: &SkimmerSpot, hovered: bool) -> Color32 {
    if hovered {
        crate::theme::CYAN
    } else if spot.callsign.is_some() {
        crate::theme::CYAN_DIM
    } else {
        Color32::from_gray(78)
    }
}

/// The on-screen width a box needs to hold just its callsign (used by the
/// fit-to-text FT8 boxes, which show the callsign only).
fn spot_content_width(p: &egui::Painter, spot: &SkimmerSpot) -> f32 {
    let mut w = 2.0 * SPOT_PAD;
    if let Some(call) = &spot.callsign {
        w += p
            .layout_no_wrap(call.clone(), FontId::monospace(SPOT_CALL_PT), Color32::WHITE)
            .size()
            .x;
    }
    w.clamp(30.0, 240.0)
}

/// Lay skimmer spots out into staggered lanes over the waterfall. Each box sits
/// to the right of its signal (offset by a leader). Visible spots are sorted by
/// x and greedily packed into the lowest lane whose footprint — from the signal
/// x through the box's right edge — clears the previous box, so nearby signals
/// stack vertically instead of overlapping. Off-view / past-cap spots are omitted.
///
/// When `fit` is set, each box is sized to its text (used for FT8 station boxes,
/// whose message is a complete fixed string); otherwise a fixed width is used
/// (the CW skimmer, whose message is a live-growing tail).
fn layout_spots(
    p: &egui::Painter,
    view: &ViewState,
    rect: &Rect,
    wf_rect: &Rect,
    spots: &[SkimmerSpot],
    fit: bool,
) -> Vec<SpotBox> {
    let mut vis: Vec<(f32, usize)> = spots
        .iter()
        .enumerate()
        .filter(|(_, s)| (view.view_lo_hz..=view.view_hi_hz).contains(&s.freq_hz))
        .map(|(i, s)| (view.freq_to_x(s.freq_hz, rect), i))
        .collect();
    vis.sort_by(|a, b| a.0.total_cmp(&b.0));

    let mut lane_right: Vec<f32> = Vec::new();
    let mut out = Vec::with_capacity(vis.len());
    for (xc, idx) in vis {
        let box_w = if fit { spot_content_width(p, &spots[idx]) } else { SPOT_BOX_W };
        let box_left = xc + SPOT_LEADER;
        // Footprint spans the signal tick through the box's right edge, so a
        // later box's tick can't land on top of an earlier box in the lane.
        let foot_right = box_left + box_w + SPOT_H_GAP;
        let mut lane = lane_right.len();
        for (k, &r) in lane_right.iter().enumerate() {
            if xc >= r {
                lane = k;
                break;
            }
        }
        if lane >= SPOT_MAX_LANES {
            continue; // too crowded here — drop the box
        }
        if lane == lane_right.len() {
            lane_right.push(0.0);
        }
        lane_right[lane] = foot_right;
        // Lanes stack away from the newest row, so the boxes sit with the fresh
        // signals they label whichever way the waterfall scrolls.
        let lane_off = SPOT_TOP_MARGIN + lane as f32 * (SPOT_BOX_H + SPOT_LANE_GAP);
        let top = if view.waterfall_flip {
            wf_rect.bottom() - lane_off - SPOT_BOX_H
        } else {
            wf_rect.top() + lane_off
        };
        out.push(SpotBox {
            rect: Rect::from_min_size(pos2(box_left, top), vec2(box_w, SPOT_BOX_H)),
            sig_x: xc,
            idx,
        });
    }
    out
}

/// Scale a colour's alpha by `a` (for fading spots out).
fn fade(c: Color32, a: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (c.a() as f32 * a).round() as u8)
}

/// Draw one skimmer box at opacity `alpha` (1 = solid, 0 = gone). In
/// `callsign_only` mode (FT8/FT4) just the callsign is shown — green when the
/// station is calling CQ, white otherwise; otherwise the CW layout is used
/// (green callsign + rolling message).
fn draw_spot_box(
    p: &egui::Painter,
    b: &SpotBox,
    spot: &SkimmerSpot,
    hovered: bool,
    alpha: f32,
    callsign_only: bool,
) {
    let rect = b.rect;
    p.rect_filled(rect, 2.0, fade(Color32::from_rgba_unmultiplied(5, 11, 18, 225), alpha));
    let border = spot_color(spot, hovered);
    p.rect_stroke(
        rect,
        2.0,
        Stroke::new(if hovered { 1.5 } else { 1.0 }, fade(border, alpha)),
        StrokeKind::Inside,
    );

    let pad = SPOT_PAD;
    let cy = rect.center().y;

    if callsign_only {
        if let Some(call) = &spot.callsign {
            // FT8 message text starts with "CQ" for callers; colour those green.
            let base =
                if spot.text.starts_with("CQ") { crate::theme::GREEN } else { Color32::WHITE };
            let cc = fade(base, alpha);
            let g = p.layout_no_wrap(call.clone(), FontId::monospace(SPOT_CALL_PT), cc);
            p.galley(pos2(rect.left() + pad, cy - g.size().y * 0.5), g, cc);
        }
        return;
    }

    let mut x = rect.left() + pad;
    if let Some(call) = &spot.callsign {
        let cc = fade(crate::theme::GREEN, alpha);
        let g = p.layout_no_wrap(call.clone(), FontId::monospace(SPOT_CALL_PT), cc);
        p.galley(pos2(x, cy - g.size().y * 0.5), g.clone(), cc);
        x += g.size().x + 6.0;
    }

    // Message area: whatever width is left of the box. The message is anchored
    // to the right edge and clipped on the left, so the newest decoded text is
    // always shown (older text slides off the left as it arrives) — steadier to
    // read than a marquee.
    let msg_rect = Rect::from_min_max(pos2(x, rect.top()), pos2(rect.right() - pad, rect.bottom()));
    if msg_rect.width() < 6.0 {
        return;
    }
    let text = if spot.text.is_empty() { "…" } else { spot.text.as_str() };
    let col = fade(crate::theme::TEXT, alpha);
    let g = p.layout_no_wrap(text.to_string(), FontId::monospace(SPOT_MSG_PT), col);
    let ty = cy - g.size().y * 0.5;
    // Left-align while it fits; once it overflows, pin the tail to the right.
    let gx = if g.size().x <= msg_rect.width() {
        msg_rect.left()
    } else {
        msg_rect.right() - g.size().x
    };
    let mp = p.with_clip_rect(msg_rect);
    mp.galley(pos2(gx, ty), g, col);
}

// --- network spot markers (DX cluster / POTA / SOTA / PSK Reporter) --------
// Anchored at the oldest edge of the waterfall (its floor, or its top when the
// waterfall is flipped) so they stay clear of the skimmer / FT8 boxes, which
// hug the newest edge.
const NET_BOX_H: f32 = 18.0;
const NET_CALL_PT: f32 = 12.5;
const NET_TAG_PT: f32 = 9.5;
const NET_PAD: f32 = 5.0;
const NET_LANE_GAP: f32 = 3.0;
const NET_BOTTOM_MARGIN: f32 = 4.0;
const NET_H_GAP: f32 = 6.0;
const NET_LEADER: f32 = 14.0;
const NET_MAX_LANES: usize = 5;

/// A laid-out network-spot box (mirrors [`SpotBox`] but bottom-anchored).
struct NetBox {
    rect: Rect,
    sig_x: f32,
    idx: usize,
}

fn net_spot_width(p: &egui::Painter, spot: &Spot) -> f32 {
    let mut w = 2.0 * NET_PAD;
    w += p
        .layout_no_wrap(spot.call.clone(), FontId::monospace(NET_CALL_PT), Color32::WHITE)
        .size()
        .x;
    w += 6.0
        + p.layout_no_wrap(
            spot.kind.label().to_string(),
            FontId::monospace(NET_TAG_PT),
            Color32::WHITE,
        )
        .size()
        .x;
    w.clamp(40.0, 220.0)
}

/// Kind tint, brightened on hover.
fn net_spot_color(spot: &Spot, hovered: bool) -> Color32 {
    let (r, g, b) = spot.kind.color();
    if hovered { Color32::from_rgb(r, g, b) } else { Color32::from_rgba_unmultiplied(r, g, b, 225) }
}

/// Lay visible network spots into bottom-anchored staggered lanes.
fn layout_net_spots(
    p: &egui::Painter,
    view: &ViewState,
    rect: &Rect,
    wf_rect: &Rect,
    spots: &[Spot],
) -> Vec<NetBox> {
    let mut vis: Vec<(f32, usize)> = spots
        .iter()
        .enumerate()
        .filter(|(_, s)| (view.view_lo_hz..=view.view_hi_hz).contains(&s.freq_hz))
        .map(|(i, s)| (view.freq_to_x(s.freq_hz, rect), i))
        .collect();
    vis.sort_by(|a, b| a.0.total_cmp(&b.0));

    let mut lane_right: Vec<f32> = Vec::new();
    let mut out = Vec::with_capacity(vis.len());
    for (xc, idx) in vis {
        let box_w = net_spot_width(p, &spots[idx]);
        let box_left = xc + NET_LEADER;
        let foot_right = box_left + box_w + NET_H_GAP;
        let mut lane = lane_right.len();
        for (k, &r) in lane_right.iter().enumerate() {
            if xc >= r {
                lane = k;
                break;
            }
        }
        if lane >= NET_MAX_LANES {
            continue;
        }
        if lane == lane_right.len() {
            lane_right.push(0.0);
        }
        lane_right[lane] = foot_right;
        // Anchored at the *oldest* edge, opposite the skimmer boxes, so the two
        // sets never collide — which end that is depends on the flip.
        let lane_off = NET_BOTTOM_MARGIN + lane as f32 * (NET_BOX_H + NET_LANE_GAP);
        let top = if view.waterfall_flip {
            wf_rect.top() + lane_off
        } else {
            wf_rect.bottom() - lane_off - NET_BOX_H
        };
        out.push(NetBox {
            rect: Rect::from_min_size(pos2(box_left, top), vec2(box_w, NET_BOX_H)),
            sig_x: xc,
            idx,
        });
    }
    out
}

/// Draw one network-spot box at opacity `alpha`: callsign in the kind colour,
/// then a small kind badge (DX/POTA/SOTA/PSK).
fn draw_net_box(p: &egui::Painter, b: &NetBox, spot: &Spot, hovered: bool, alpha: f32) {
    let rect = b.rect;
    p.rect_filled(rect, 2.0, fade(Color32::from_rgba_unmultiplied(6, 12, 20, 232), alpha));
    let border = net_spot_color(spot, hovered);
    p.rect_stroke(
        rect,
        2.0,
        Stroke::new(if hovered { 1.5 } else { 1.0 }, fade(border, alpha)),
        StrokeKind::Inside,
    );
    let cy = rect.center().y;
    let mut x = rect.left() + NET_PAD;
    let call_col = fade(border, alpha);
    let g = p.layout_no_wrap(spot.call.clone(), FontId::monospace(NET_CALL_PT), call_col);
    p.galley(pos2(x, cy - g.size().y * 0.5), g.clone(), call_col);
    x += g.size().x + 6.0;
    let tag_col = fade(Color32::from_gray(180), alpha);
    let tg =
        p.layout_no_wrap(spot.kind.label().to_string(), FontId::monospace(NET_TAG_PT), tag_col);
    if x + tg.size().x <= rect.right() - NET_PAD {
        p.galley(pos2(x, cy - tg.size().y * 0.5), tg, tag_col);
    }
}

/// Tune the active VFO onto a network spot and set its mode. CW is dialed a
/// sidetone-pitch below so the signal lands in the CW passband (as click-tune
/// on a skimmer spot does).
fn tune_to_net_spot(spot: &Spot, state: &RadioState, cmds: &mut Vec<Command>) {
    match spot.radio_mode() {
        Some(Mode::Cw) => {
            let (lo, hi) = Mode::Cw.default_filter();
            let pitch = ((lo + hi) * 0.5) as f64;
            cmds.push(Command::SetVfo { vfo: state.active_vfo, hz: spot.freq_hz - pitch });
            cmds.push(Command::SetMode { rx: RxId::Main, mode: Mode::Cw });
        }
        Some(m) => {
            cmds.push(Command::SetVfo { vfo: state.active_vfo, hz: spot.freq_hz });
            cmds.push(Command::SetMode { rx: RxId::Main, mode: m });
        }
        None => cmds.push(Command::SetVfo { vfo: state.active_vfo, hz: spot.freq_hz }),
    }
}

/// Decaying peak-hold trace, reset whenever the frame mapping changes.
#[derive(Default)]
pub struct PeakHold {
    bins: Vec<f32>,
    center: f64,
    span: f64,
    /// Sequence of the frame last folded in, so decay runs once per *new*
    /// frame rather than once per repaint (repaint rate must not change the
    /// decay speed).
    last_seq: Option<u32>,
    /// Bumped whenever `bins` change outside the seq progression (mapping
    /// change, clear) — part of the trace-cache key.
    generation: u32,
}

/// dB-scale u8 units per new spectrum frame (~3.5 dB/s at 30 fps over a
/// 100 dB range).
const PEAK_DECAY: f32 = 0.3;

impl PeakHold {
    fn update(&mut self, f: &SpectrumFrame) {
        if self.last_seq == Some(f.seq) {
            return; // same frame redrawn — nothing new to fold in
        }
        self.last_seq = Some(f.seq);
        let mapping_changed = self.bins.len() != f.bins.len()
            || (self.center - f.center_hz).abs() > f.span_hz * 1e-6
            || (self.span - f.span_hz).abs() > f.span_hz * 1e-6;
        if mapping_changed {
            self.center = f.center_hz;
            self.span = f.span_hz;
            self.bins = f.bins.iter().map(|&b| b as f32).collect();
            self.generation = self.generation.wrapping_add(1);
            return;
        }
        for (p, &b) in self.bins.iter_mut().zip(&f.bins) {
            *p = (b as f32).max(*p - PEAK_DECAY);
        }
    }

    fn clear(&mut self) {
        self.bins.clear();
        self.last_seq = None;
        self.generation = self.generation.wrapping_add(1);
    }
}

/// Screen-space spectrum polylines, recomputed only when the underlying frame,
/// viewport, or rect actually change — pure repaints reuse the cached points.
#[derive(Default)]
pub struct TraceCache {
    live: TraceEntry,
    hold: TraceEntry,
}

#[derive(Default)]
struct TraceEntry {
    key: Option<TraceKey>,
    points: Vec<Pos2>,
}

/// Everything the per-pixel trace math depends on. Float fields are compared
/// as exact bits — any change must invalidate. (`db_floor`/`db_ceil` need no
/// entry: they reach the trace via the engine's u8 mapping, i.e. a new seq.)
#[derive(Clone, Copy, PartialEq)]
struct TraceKey {
    seq: u32,
    generation: u32,
    view_lo: u64,
    view_hi: u64,
    rect: [u32; 4],
}

fn trace_key(f: &SpectrumFrame, generation: u32, view: &ViewState, rect: &Rect) -> TraceKey {
    TraceKey {
        seq: f.seq,
        generation,
        view_lo: view.view_lo_hz.to_bits(),
        view_hi: view.view_hi_hz.to_bits(),
        rect: [
            rect.left().to_bits(),
            rect.top().to_bits(),
            rect.right().to_bits(),
            rect.bottom().to_bits(),
        ],
    }
}

impl TraceEntry {
    /// Recompute the polyline only when `key` changed; hand back a copy for
    /// `Shape::line` (a memcpy — far cheaper than the per-pixel f64 math).
    fn points_for(&mut self, key: TraceKey, compute: impl FnOnce() -> Vec<Pos2>) -> Vec<Pos2> {
        if self.key != Some(key) {
            self.points = compute();
            self.key = Some(key);
        }
        self.points.clone()
    }
}

#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut Ui,
    view: &mut ViewState,
    state: &mut RadioState,
    frame: Option<&Arc<SpectrumFrame>>,
    peaks: &mut PeakHold,
    smooth: &mut SpectrumSmooth,
    trace: &mut TraceCache,
    skimmer: &[SkimmerSpot],
    alpha: &[f32],
    net_spots: &[Spot],
    net_alpha: &[f32],
    clicked_spot: &mut Option<Spot>,
    wheel: WheelSettings,
    wf: WfTuning,
    cmds: &mut Vec<Command>,
) {
    show_ext(
        ui,
        view,
        state,
        frame,
        peaks,
        smooth,
        trace,
        None,
        sdroxide_types::DxpedMode::Normal,
        false,
        &[],
        skimmer,
        alpha,
        net_spots,
        net_alpha,
        clicked_spot,
        wheel,
        wf,
        cmds,
    );
}

/// `show` with an optional digital-mode audio marker. When `digi_audio_hz`
/// is `Some`, left-click sets the FT8/FT4 audio TX frequency instead of the
/// VFO, and a marker is drawn at `dial + audio_hz`.
///
/// `skimmer` are the overlay boxes (CW skimmer spots, or FT8 station callsigns
/// in digital mode) and `alpha` is a parallel per-box opacity for fade-out.
pub fn show_ext(
    ui: &mut Ui,
    view: &mut ViewState,
    state: &mut RadioState,
    frame: Option<&Arc<SpectrumFrame>>,
    peaks: &mut PeakHold,
    smooth: &mut SpectrumSmooth,
    trace: &mut TraceCache,
    digi_audio_hz: Option<f32>,
    // FT8 DXpedition role. Anything but `Normal` shades the two halves of the
    // passband the pile-up divides itself into, with the half we operate in
    // named — a Hound calling down among the Fox's signals is the single most
    // common way to be unworkable, and it is invisible without this.
    dxped: sdroxide_types::DxpedMode,
    // FT8/FT4: the engine is choosing the transmit frequency, so clicking a
    // station label must not drag it onto that station.
    auto_tx_freq: bool,
    // Extra audio-offset tuning lines (Hz relative to the dial), e.g. the RTTY
    // mark/space pair. Drawn as thin amber lines to aid exact tuning.
    markers: &[f32],
    skimmer: &[SkimmerSpot],
    alpha: &[f32],
    // Network spots (DX cluster / POTA / SOTA / PSK) with a parallel fade alpha.
    // On click, the clicked spot is returned via `clicked_spot` (and the VFO is
    // tuned + mode set here) so the app can pre-fill a log entry.
    net_spots: &[Spot],
    net_alpha: &[f32],
    clicked_spot: &mut Option<Spot>,
    // Operator's pointer preferences: what the wheel does (plain and with
    // Shift), whether left-drag tunes, and the click-tune rounding.
    wheel: WheelSettings,
    wf: WfTuning,
    cmds: &mut Vec<Command>,
) {
    let rect = ui.available_rect_before_wrap();
    let resp = ui.allocate_rect(rect, Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, Color32::from_gray(8));

    let Some(f) = frame.filter(|f| f.span_hz > 0.0 && !f.bins.is_empty()) else {
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            "waiting for spectrum…",
            FontId::proportional(16.0),
            Color32::GRAY,
        );
        return;
    };

    // The pan/zoom bounds are the full device passband (frames may cover
    // only the zoomed viewport).
    let dev_center = state.center_hz;
    let dev_span = state.sample_rate;
    // Both receivers are DDCs on this one IQ stream, so nothing outside these
    // edges can be tuned — the sub gets clamped to them.
    let dev_lo = dev_center - dev_span / 2.0;
    let dev_hi = dev_center + dev_span / 2.0;
    if view.is_unset() {
        view.fit(dev_center, dev_span);
    }

    // Layout first — interactions depend on which strip the pointer is in.
    // A collapsed spectrum shows only the waterfall.
    let frac = view.effective_spectrum_fraction();
    let spec_h = if frac <= 0.0 { 0.0 } else { ((rect.height() - SCALE_H) * frac).max(40.0) };
    let spec_rect = Rect::from_min_size(rect.min, vec2(rect.width(), spec_h));
    let scale_rect =
        Rect::from_min_size(pos2(rect.left(), spec_rect.bottom()), vec2(rect.width(), SCALE_H));
    let wf_rect = Rect::from_min_max(pos2(rect.left(), scale_rect.bottom()), rect.max);

    // Skimmer boxes are laid out up front so the click hit-test (below) and the
    // draw pass (bottom) agree on their rects. FT8 (digital) boxes fit their
    // text; CW skimmer boxes use a fixed width for their live-growing tail.
    let spot_boxes =
        layout_spots(&painter, view, &rect, &wf_rect, skimmer, digi_audio_hz.is_some());
    let net_boxes = layout_net_spots(&painter, view, &rect, &wf_rect, net_spots);

    // --- interactions -----------------------------------------------------
    // Model: grabbing a filter edge (left button, spectrum strip) always
    // wins and only moves the edge. Otherwise: left-drag pans the view AND
    // drags the tuning with it; right-drag pans the view only.
    let vfo_hz = state.rx_freq_hz();
    let sub_hz = state.sub_rx_hz;
    let sub_on = state.sub_rx_enabled;
    let px0 = view.freq_to_x(vfo_hz + state.rx[0].filter_lo as f64, &rect);
    let px1 = view.freq_to_x(vfo_hz + state.rx[0].filter_hi as f64, &rect);
    let spx0 = view.freq_to_x(sub_hz + state.rx[1].filter_lo as f64, &rect);
    let spx1 = view.freq_to_x(sub_hz + state.rx[1].filter_hi as f64, &rect);
    let sub_marker_x = view.freq_to_x(sub_hz, &rect);

    // Which passband edge the pointer can grab: the receiver it belongs to, and
    // whether it is the high edge.
    let edge_at = |p: egui::Pos2| -> Option<(RxId, bool)> {
        // Grabbable in either the spectrum or the waterfall strip, so the edges
        // can still be dragged when the spectrum line is collapsed.
        if !(spec_rect.contains(p) || wf_rect.contains(p)) {
            return None;
        }
        // Nearest edge wins, and the sub's edges break a tie (they come last):
        // where the two passbands overlap, the pointer means the receiver the
        // operator just parked there, not the one they have been on all along.
        let mut best = None;
        let mut best_d = EDGE_GRAB_PX;
        for (x, rx, hi) in [
            (px0, RxId::Main, false),
            (px1, RxId::Main, true),
            (spx0, RxId::Sub, false),
            (spx1, RxId::Sub, true),
        ] {
            if rx == RxId::Sub && !sub_on {
                continue;
            }
            let d = (p.x - x).abs();
            if d <= best_d {
                best_d = d;
                best = Some((rx, hi));
            }
        }
        best
    };

    // Dragging inside a receiver's passband tunes that receiver. The main one
    // needs no special case — a left-drag anywhere already slides the spectrum
    // and takes the dial with it — so this only has to claim the sub's, plus
    // its tuning line, which in SSB sits outside the passband it opens onto.
    let sub_grab_at = |p: egui::Pos2| -> bool {
        if !sub_on || !(spec_rect.contains(p) || wf_rect.contains(p)) {
            return false;
        }
        (spx0.min(spx1)..=spx0.max(spx1)).contains(&p.x)
            || (p.x - sub_marker_x).abs() < MARKER_GRAB_PX
    };

    let edge_id = ui.id().with("pb-edge");
    let mut edge: Option<(RxId, bool)> = ui.data(|d| d.get_temp(edge_id)).unwrap_or(None);
    let hover_edge = resp.hover_pos().and_then(edge_at);

    let sub_drag_id = ui.id().with("sub-drag");
    let mut sub_drag: bool = ui.data(|d| d.get_temp(sub_drag_id)).unwrap_or(false);
    let hover_sub = resp.hover_pos().map(sub_grab_at).unwrap_or(false) && hover_edge.is_none();

    // The frequency-scale strip doubles as the spectrum/waterfall resize grip:
    // a vertical drag there changes the spectrum height.
    let resize_id = ui.id().with("spec-resize");
    let mut resizing: bool = ui.data(|d| d.get_temp(resize_id)).unwrap_or(false);
    let hover_resize = resp.hover_pos().map(|p| scale_rect.contains(p)).unwrap_or(false);

    // Shift+drag bandwidth measurement: remembers the frequency where the drag
    // started; `None` when not measuring.
    let measure_id = ui.id().with("bw-measure");
    let mut measuring: Option<f64> = ui.data(|d| d.get_temp(measure_id)).unwrap_or(None);
    // After releasing, the frozen span lingers and fades out over `BW_FADE_SECS`:
    // (start_hz, end_hz, release_time_secs).
    let fade_id = ui.id().with("bw-fade");
    let mut bw_fade: Option<(f64, f64, f64)> = ui.data(|d| d.get_temp(fade_id)).unwrap_or(None);

    // Dial inertia: a tuning drag released while still moving leaves the dial
    // coasting to a stop. Written back once, at the end of the interaction
    // handling below.
    let fling_id = ui.id().with("dial-fling");
    let mut fling: Option<Fling> = ui.data(|d| d.get_temp(fling_id)).unwrap_or(None);
    // Set when a press has stopped a coast, so the click it becomes is swallowed:
    // catching a spinning knob means "stop here", never "tune to where my hand
    // landed". Lives across the press/release frame pair.
    let stop_click_id = ui.id().with("fling-stop-click");
    let mut stop_click: bool = ui.data(|d| d.get_temp(stop_click_id)).unwrap_or(false);

    if resp.drag_started_by(egui::PointerButton::Primary) {
        // Decide from the PRESS position, not the current pointer position —
        // by the time the drag threshold trips, the pointer may already have
        // left the grab zone.
        let origin = ui.input(|i| i.pointer.press_origin());
        if ui.input(|i| i.modifiers.shift) {
            // Shift+drag measures bandwidth — overrides edge grab / resize / pan,
            // and cancels any lingering fade from a previous measurement.
            measuring = origin.map(|p| view.x_to_freq(p.x, &rect));
            edge = None;
            sub_drag = false;
            resizing = false;
            bw_fade = None;
        } else {
            edge = origin.and_then(edge_at);
            sub_drag = edge.is_none() && origin.map(sub_grab_at).unwrap_or(false);
            resizing = edge.is_none()
                && !sub_drag
                && origin.map(|p| scale_rect.contains(p)).unwrap_or(false);
            measuring = None;
        }
        ui.data_mut(|d| {
            d.insert_temp(edge_id, edge);
            d.insert_temp(sub_drag_id, sub_drag);
            d.insert_temp(resize_id, resizing);
            d.insert_temp(measure_id, measuring);
            d.insert_temp(fade_id, bw_fade);
        });
    }
    if resp.drag_stopped() {
        // Hand the drag's momentum to the dial it was turning, before the flags
        // that say which dial that was are cleared below. Only the two tuning
        // gestures have a flywheel: a filter edge, the resize grip, the
        // measurement ruler and a pan that does not tune (right-drag, or
        // left-drag with `drag_tunes` off) have nothing to spin. egui
        // deliberately keeps the pointer's final velocity alive for the frame a
        // drag ends on, which is exactly the release speed this needs.
        let dial = if !resp.drag_stopped_by(egui::PointerButton::Primary) {
            None
        } else if sub_drag {
            Some(true)
        } else if wheel.drag_tunes && edge.is_none() && !resizing && measuring.is_none() {
            Some(false)
        } else {
            None
        };
        if let Some(sub) = dial {
            let vx = ui.input(|i| i.pointer.velocity().x);
            fling = (vx.abs() >= FLING_MIN_LAUNCH).then_some(Fling { sub, vx });
        }
        // If we were measuring, freeze the span and start its fade-out.
        if let Some(start_hz) = measuring {
            let end_hz =
                resp.interact_pointer_pos().map(|p| view.x_to_freq(p.x, &rect)).unwrap_or(start_hz);
            bw_fade = Some((start_hz, end_hz, ui.input(|i| i.time)));
        }
        edge = None;
        sub_drag = false;
        resizing = false;
        measuring = None;
        ui.data_mut(|d| {
            d.insert_temp(edge_id, edge);
            d.insert_temp(sub_drag_id, sub_drag);
            d.insert_temp(resize_id, resizing);
            d.insert_temp(measure_id, measuring);
            d.insert_temp(fade_id, bw_fade);
        });
    }
    if measuring.is_some() || (resp.hovered() && ui.input(|i| i.modifiers.shift)) {
        ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
    } else if hover_edge.is_some() || edge.is_some() {
        ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
    } else if sub_drag {
        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
    } else if hover_sub {
        ui.ctx().set_cursor_icon(CursorIcon::Grab);
    } else if hover_resize || resizing {
        ui.ctx().set_cursor_icon(CursorIcon::ResizeVertical);
    }

    // Secondary-button panning is tracked manually — egui only registers
    // widget drags started with the primary button.
    let sec_id = ui.id().with("sec-pan");
    let mut sec_pan: bool = ui.data(|d| d.get_temp(sec_id)).unwrap_or(false);
    let (sec_down, press_origin, pointer_delta) =
        ui.input(|i| (i.pointer.secondary_down(), i.pointer.press_origin(), i.pointer.delta()));
    let sec_pan_before = sec_pan;
    if sec_down && !sec_pan && press_origin.is_some_and(|p| rect.contains(p)) {
        sec_pan = true;
    }
    if !sec_down {
        sec_pan = false;
    }
    if sec_pan != sec_pan_before {
        ui.data_mut(|d| d.insert_temp(sec_id, sec_pan));
    }

    // A hand back on a spinning knob stops it dead. This catches the press
    // itself, not the drag it may become: a click-tune, or the first few pixels
    // before the drag threshold trips, must not have the old coast running
    // underneath them.
    if fling.is_some()
        && ui.input(|i| i.pointer.any_pressed())
        && press_origin.is_some_and(|p| rect.contains(p))
    {
        fling = None;
        stop_click = true;
    }

    if measuring.is_some() && resp.dragged_by(egui::PointerButton::Primary) {
        // Bandwidth measurement — no tuning or panning; drawn in the overlay
        // pass at the end.
    } else if resizing && resp.dragged_by(egui::PointerButton::Primary) {
        // Spectrum/waterfall resize — set the spectrum height from the pointer.
        if let Some(p) = resp.interact_pointer_pos() {
            let usable = (rect.height() - SCALE_H).max(1.0);
            view.spectrum_fraction = ((p.y - rect.top()) / usable).clamp(0.10, 0.85);
            view.spectrum_collapsed = false; // dragging it open implies visible
        }
    } else if let (Some((rx, is_hi)), true) = (edge, resp.dragged_by(egui::PointerButton::Primary))
    {
        // Filter edge drag — exclusive, never pans. Edges are relative to the
        // receiver they belong to, so the sub's grips measure from where the
        // sub sits, not from the dial.
        if let Some(p) = resp.interact_pointer_pos() {
            let anchor = if rx == RxId::Main { vfo_hz } else { sub_hz };
            let rel = view.x_to_freq(p.x, &rect) - anchor;
            let r = &mut state.rx[rx.index()];
            let max_hz = r.mode.max_filter_hz() as f64;
            let (mut lo, mut hi) = (r.filter_lo as f64, r.filter_hi as f64);
            if is_hi {
                hi = rel.clamp(lo + 50.0, max_hz);
            } else {
                lo = rel.clamp(-max_hz, hi - 50.0);
            }
            // Optimistic echo so the grip tracks the pointer exactly.
            (r.filter_lo, r.filter_hi) = (lo as f32, hi as f32);
            cmds.push(Command::SetFilter { rx, lo: lo as f32, hi: hi as f32 });
        }
    } else if sub_drag && resp.dragged_by(egui::PointerButton::Primary) {
        // Sub-receiver tuning drag — exclusive, never pans, and clamped to the
        // device passband because that is all its DDC can reach.
        //
        // By the pointer's *delta*, not its absolute position: the grab is the
        // whole passband, so snapping the receiver's centre onto the pointer
        // would jump it by half a filter width the moment the drag began.
        // The sub follows the pointer rather than the grab-the-content sense a
        // pan has — the operator has hold of the receiver itself.
        let dhz = resp.drag_delta().x as f64 * view.span() / rect.width() as f64;
        let hz = (state.sub_rx_hz + dhz).clamp(dev_lo, dev_hi);
        state.sub_rx_hz = hz; // optimistic echo, as for the filter grips
        cmds.push(Command::SetSubRxFreq(hz));
    } else if sec_pan {
        // Right-drag: pan only, grab-the-content semantics.
        let dhz = -pointer_delta.x as f64 * view.span() / rect.width() as f64;
        view.view_lo_hz += dhz;
        view.view_hi_hz += dhz;
    } else if resp.dragged_by(egui::PointerButton::Primary) {
        // Left-drag: grab the spectrum and slide it — content follows the
        // mouse, and the tuning follows the content (dragging right tunes
        // down). The view pans along so the VFO marker keeps its place.
        // With `drag_tunes` off it pans only, like the right-drag.
        let dhz = -resp.drag_delta().x as f64 * view.span() / rect.width() as f64;
        view.view_lo_hz += dhz;
        view.view_hi_hz += dhz;
        if wheel.drag_tunes {
            let hz = (state.active_freq_hz() + dhz).max(0.0);
            match state.active_vfo {
                Vfo::A => state.vfo_a_hz = hz, // optimistic echo
                Vfo::B => state.vfo_b_hz = hz,
            }
            cmds.push(Command::SetVfo { vfo: state.active_vfo, hz });
        }
    } else {
        // --- wheel (zoom or tune) / click-tune -----------------------------
        if let Some(pos) = resp.hover_pos() {
            let (scroll, shift) = ui.input(|i| (i.smooth_scroll_delta.y, i.modifiers.shift));
            let scroll = if wheel.invert { -scroll } else { scroll };
            let act = if shift { wheel.wheel_shift } else { wheel.wheel };
            if scroll.abs() > 0.1 {
                // Zooming or wheel-tuning is a fresh command to the dial; a coast
                // still running would fight it.
                fling = None;
                match act {
                    WheelAction::Zoom => {
                        // Zoom around the cursor; scroll up = zoom in.
                        let rate = wheel.zoom_rate.clamp(0.1, 5.0) as f64;
                        let factor = 0.998f64.powf(scroll as f64 * 2.0 * rate);
                        let fpos = view.x_to_freq(pos.x, &rect);
                        let lo = fpos - (fpos - view.view_lo_hz) * factor;
                        let hi = fpos + (view.view_hi_hz - fpos) * factor;
                        let min_span = (dev_span / 1024.0).max(1_000.0);
                        if hi - lo >= min_span {
                            view.view_lo_hz = lo;
                            view.view_hi_hz = hi;
                        }
                    }
                    WheelAction::Tune => {
                        // Same optimistic echo as the drag arm above, so the
                        // readout keeps up with a fast scroll.
                        let detents = (scroll / SCROLL_PER_DETENT) as f64;
                        let hz = (state.active_freq_hz() + detents * wheel.tune_step_hz).max(0.0);
                        match state.active_vfo {
                            Vfo::A => state.vfo_a_hz = hz,
                            Vfo::B => state.vfo_b_hz = hz,
                        }
                        cmds.push(Command::SetVfo { vfo: state.active_vfo, hz });
                    }
                    WheelAction::None => {}
                }
            }
        }
        if resp.clicked() && !stop_click {
            if let Some(pos) = resp.interact_pointer_pos() {
                let step = if wheel.click_tune_step_hz > 0.0 {
                    wheel.click_tune_step_hz
                } else {
                    CLICK_TUNE_STEP
                };
                if ui.input(|i| i.modifiers.shift) {
                    // Shift-click places the *second* receiver: the sub when it
                    // is running, VFO B otherwise — what this gesture meant
                    // before the sub had a frequency of its own.
                    //
                    // Taken ahead of the spot boxes and the digital-mode branch
                    // so it means the same thing everywhere on the panadapter.
                    // That matters most in FT8, where a plain click moves the
                    // transmit offset instead of tuning: without this there
                    // would be no way to park the sub on a signal at all.
                    let hz = net_boxes
                        .iter()
                        .find(|b| b.rect.contains(pos))
                        .map(|b| net_spots[b.idx].freq_hz)
                        .or_else(|| {
                            spot_boxes
                                .iter()
                                .find(|b| b.rect.contains(pos))
                                .map(|b| skimmer[b.idx].freq_hz)
                        })
                        .unwrap_or_else(|| (view.x_to_freq(pos.x, &rect) / step).round() * step);
                    if state.sub_rx_enabled {
                        let hz = hz.clamp(dev_lo, dev_hi);
                        state.sub_rx_hz = hz; // optimistic echo
                        cmds.push(Command::SetSubRxFreq(hz));
                    } else {
                        state.vfo_b_hz = hz;
                        cmds.push(Command::SetVfo { vfo: Vfo::B, hz });
                    }
                } else if let Some(nb) = net_boxes.iter().find(|b| b.rect.contains(pos)) {
                    // Network spot: tune + set mode, and hand the spot back so the
                    // app can pre-fill a log entry (and optionally look it up).
                    let spot = &net_spots[nb.idx];
                    tune_to_net_spot(spot, state, cmds);
                    *clicked_spot = Some(spot.clone());
                } else if let Some(sb) = spot_boxes.iter().find(|b| b.rect.contains(pos)) {
                    let spot = &skimmer[sb.idx];
                    let spot_hz = spot.freq_hz;
                    if digi_audio_hz.is_some() {
                        // FT8 station box. Moving our transmit onto the station
                        // is what Auto TX FRQ exists to avoid — they transmit in
                        // the period opposite ours, so their frequency says
                        // nothing about who is there when we key. With it on the
                        // label is a label; with it off it tunes, as before.
                        if !auto_tx_freq {
                            let audio = (spot_hz - state.rx_freq_hz()) as f32;
                            cmds.push(Command::SetDigiAudioFreq(audio.clamp(200.0, 3500.0)));
                        }
                    } else {
                        // Skimmer spot: switch to the spot's mode and tune onto it.
                        match spot.kind {
                            SkimmerKind::Cw => {
                                // Dial a sidetone-pitch below so it lands in the
                                // CW filter (CW is USB-side, ~700 Hz passband).
                                let (lo, hi) = Mode::Cw.default_filter();
                                let pitch = ((lo + hi) * 0.5) as f64;
                                cmds.push(Command::SetVfo {
                                    vfo: state.active_vfo,
                                    hz: spot_hz - pitch,
                                });
                                cmds.push(Command::SetMode { rx: RxId::Main, mode: Mode::Cw });
                            }
                            SkimmerKind::Psk | SkimmerKind::Rtty => {
                                // Put the signal at a fixed audio offset and open
                                // the PSK/RTTY panel there for a clean decode.
                                const AF: f64 = 1500.0;
                                cmds.push(Command::SetVfo {
                                    vfo: state.active_vfo,
                                    hz: spot_hz - AF,
                                });
                                cmds.push(Command::SetMode {
                                    rx: RxId::Main,
                                    mode: spot.kind.mode(),
                                });
                                cmds.push(Command::SetDigiAudioFreq(AF as f32));
                            }
                        }
                    }
                } else if digi_audio_hz.is_some() {
                    // Digital mode: set the audio TX offset, not the VFO.
                    let audio = (view.x_to_freq(pos.x, &rect) - state.rx_freq_hz()) as f32;
                    cmds.push(Command::SetDigiAudioFreq(audio.clamp(200.0, 3500.0)));
                } else {
                    let hz = (view.x_to_freq(pos.x, &rect) / step).round() * step;
                    cmds.push(Command::SetVfo { vfo: state.active_vfo, hz });
                }
            }
        }
    }

    // --- dial inertia -----------------------------------------------------
    // Carry a released tuning drag on, decelerating, so the dial coasts to rest
    // instead of stopping the instant the button comes up. Both terms of the
    // deceleration are in screen points, and the Hz step is derived from the
    // span in force *this* frame, so a coast that is zoomed into mid-flight
    // slows down in Hz with it and keeps sliding the display at the same rate.

    // A sub receiver switched off mid-coast leaves no dial to turn, and its
    // momentum must not be waiting when it comes back.
    if fling.is_some_and(|f| f.sub && !sub_on) {
        fling = None;
    }
    if let Some(f) = fling.filter(|_| !resp.dragged_by(egui::PointerButton::Primary) && !sec_pan) {
        // Clamped: a stalled frame (a mode change reloading the FFT, say) must
        // not teleport the dial across the band.
        let dt = ui.input(|i| i.stable_dt).clamp(0.0, 0.1);
        let vx = fling_decay(f.vx, dt);
        if vx.abs() < FLING_STOP {
            fling = None;
        } else {
            let dhz = (vx * dt) as f64 * view.span() / rect.width() as f64;
            fling = Some(Fling { vx, ..f });
            if f.sub {
                // The sub follows the pointer, so it coasts the same way it was
                // dragged; the device passband is the end stop its DDC cannot
                // reach past, and hitting it parks the dial.
                let hz = (state.sub_rx_hz + dhz).clamp(dev_lo, dev_hi);
                if hz == state.sub_rx_hz {
                    fling = None;
                } else {
                    state.sub_rx_hz = hz; // optimistic echo, as during the drag
                    cmds.push(Command::SetSubRxFreq(hz));
                }
            } else {
                // Grab-the-content sense, matching the left-drag: the view slides
                // with the dial so the VFO marker keeps its place on screen.
                view.view_lo_hz -= dhz;
                view.view_hi_hz -= dhz;
                let hz = (state.active_freq_hz() - dhz).max(0.0);
                match state.active_vfo {
                    Vfo::A => state.vfo_a_hz = hz,
                    Vfo::B => state.vfo_b_hz = hz,
                }
                cmds.push(Command::SetVfo { vfo: state.active_vfo, hz });
                if hz == 0.0 {
                    fling = None; // ran into the bottom of the spectrum
                }
            }
            // The coast is animation, not a response to input: without this it
            // would advance only when something else happened to repaint.
            ui.ctx().request_repaint();
        }
    }
    // The swallow only covers the one press/release pair that caught the dial.
    if ui.input(|i| i.pointer.any_released()) {
        stop_click = false;
    }
    ui.data_mut(|d| {
        d.insert_temp(fling_id, fling);
        d.insert_temp(stop_click_id, stop_click);
    });

    view.clamp_to(dev_center, dev_span);

    // --- drawing ----------------------------------------------------------
    // Recompute after this frame's pan/tune/edge updates.
    let vfo_hz = state.rx_freq_hz();
    let px0 = view.freq_to_x(vfo_hz + state.rx[0].filter_lo as f64, &rect);
    let px1 = view.freq_to_x(vfo_hz + state.rx[0].filter_hi as f64, &rect);
    let sub_hz = state.sub_rx_hz;
    let spx0 = view.freq_to_x(sub_hz + state.rx[1].filter_lo as f64, &rect);
    let spx1 = view.freq_to_x(sub_hz + state.rx[1].filter_hi as f64, &rect);

    if spec_h > 1.0 {
        // Optional vertical background gradient behind the grid and trace.
        if let Some((top, bottom)) = wf.gradient {
            let mut mesh = egui::epaint::Mesh::default();
            let uv = egui::epaint::WHITE_UV;
            mesh.vertices.push(egui::epaint::Vertex { pos: spec_rect.left_top(), uv, color: top });
            mesh.vertices.push(egui::epaint::Vertex { pos: spec_rect.right_top(), uv, color: top });
            mesh.vertices.push(egui::epaint::Vertex {
                pos: spec_rect.right_bottom(),
                uv,
                color: bottom,
            });
            mesh.vertices.push(egui::epaint::Vertex {
                pos: spec_rect.left_bottom(),
                uv,
                color: bottom,
            });
            mesh.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
            painter.add(Shape::mesh(mesh));
        }
        draw_grid(&painter, view, &spec_rect);
        if view.peak_hold {
            peaks.update(f);
            let key = trace_key(f, peaks.generation, view, &spec_rect);
            let pts = trace
                .hold
                .points_for(key, || compute_trace(view, f, Some(&peaks.bins), &spec_rect));
            painter.add(Shape::line(
                pts,
                Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 220, 90, 170)),
            ));
        } else {
            peaks.clear();
        }
        // Live trace: UI-smoothed (per the spectrum-speed setting) so the line's
        // reaction speed is independent of the un-averaged waterfall detail.
        smooth.update(f, wf.spectrum_alpha);
        let key = trace_key(f, smooth.generation, view, &spec_rect);
        let pts =
            trace.live.points_for(key, || compute_trace(view, f, Some(&smooth.bins), &spec_rect));
        painter.add(Shape::line(pts, Stroke::new(1.0, Color32::from_rgb(120, 220, 255))));
    }
    draw_scale(&painter, view, &scale_rect);

    // Resize grip: a short centred handle in the scale strip, brightening when
    // hovered/dragged, so the divider is discoverable.
    {
        let cy = scale_rect.top() + 1.5;
        let cx = scale_rect.center().x;
        let col =
            if hover_resize || resizing { crate::theme::CYAN } else { Color32::from_gray(70) };
        for dx in [-16.0f32, 0.0, 16.0] {
            painter.line_segment(
                [pos2(cx + dx - 6.0, cy), pos2(cx + dx + 6.0, cy)],
                Stroke::new(2.0, col),
            );
        }
    }

    // Waterfall (GPU): viewport expressed in frame coordinates.
    let base = f.center_hz - f.span_hz / 2.0;
    let u_lo = ((view.view_lo_hz - base) / f.span_hz) as f32;
    let u_hi = ((view.view_hi_hz - base) / f.span_hz) as f32;
    ui.painter().add(crate::egui_wgpu::Callback::new_paint_callback(
        wf_rect,
        WaterfallCallback {
            // Arc::clone, not a bins deep-clone — keep it explicit.
            frame: Some(Arc::clone(f)),
            u_lo,
            u_hi,
            rows_visible: wf_rect.height(),
            lut: wf.palette,
            rows_to_write: wf.rows_to_write,
            flip: view.waterfall_flip,
        },
    ));

    // Bandplan strip along the bottom of the waterfall (over the GPU layer).
    crate::widgets::bandplan::overlay(&painter, view, &wf_rect);

    // --- VFO markers + passband shading -----------------------------------
    let in_view = |hz: f64| (view.view_lo_hz..=view.view_hi_hz).contains(&hz);

    let (x0c, x1c) = (px0.clamp(rect.left(), rect.right()), px1.clamp(rect.left(), rect.right()));
    if x1c > x0c {
        // Passband shading on the spectrum strip and (fainter) on the
        // waterfall, so the filter width stays visible when collapsed.
        if spec_h > 1.0 {
            painter.rect_filled(
                Rect::from_min_max(pos2(x0c, spec_rect.top()), pos2(x1c, spec_rect.bottom())),
                0.0,
                Color32::from_rgba_unmultiplied(255, 90, 90, 26),
            );
        }
        painter.rect_filled(
            Rect::from_min_max(pos2(x0c, wf_rect.top()), pos2(x1c, wf_rect.bottom())),
            0.0,
            Color32::from_rgba_unmultiplied(255, 90, 90, 16),
        );
    }
    // Edge grips (brighter when grabbable). Drawn on both strips so the
    // filter edges remain visible with the spectrum line collapsed.
    for (x, e) in [(px0, (RxId::Main, false)), (px1, (RxId::Main, true))] {
        if rect.x_range().contains(x) {
            let hot = hover_edge == Some(e) || edge == Some(e);
            let w = if hot { 2.0 } else { 1.0 };
            if spec_h > 1.0 {
                let color =
                    if hot { Color32::from_rgb(255, 170, 90) } else { Color32::from_gray(90) };
                painter.vline(x, spec_rect.y_range(), Stroke::new(w, color));
            }
            let wf_color = if hot {
                Color32::from_rgba_unmultiplied(255, 170, 90, 200)
            } else {
                Color32::from_rgba_unmultiplied(150, 160, 170, 120)
            };
            painter.vline(x, wf_rect.y_range(), Stroke::new(w, wf_color));
        }
    }

    if in_view(vfo_hz) {
        let x = view.freq_to_x(vfo_hz, &rect);
        painter.vline(x, spec_rect.y_range(), Stroke::new(1.0, Color32::from_rgb(255, 60, 60)));
        painter.vline(
            x,
            wf_rect.y_range(),
            Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 60, 60, 140)),
        );
    }

    // --- sub receiver ------------------------------------------------------
    // Drawn with the same vocabulary as the main receiver — passband wash, edge
    // grips, tuning line — so a second receiver reads as a receiver and not as
    // one more annotation. Only its colour differs.
    if sub_on {
        let (c_r, c_g, c_b) = (SUB_COLOR.r(), SUB_COLOR.g(), SUB_COLOR.b());
        let (sx0c, sx1c) =
            (spx0.clamp(rect.left(), rect.right()), spx1.clamp(rect.left(), rect.right()));
        if sx1c > sx0c {
            if spec_h > 1.0 {
                painter.rect_filled(
                    Rect::from_min_max(pos2(sx0c, spec_rect.top()), pos2(sx1c, spec_rect.bottom())),
                    0.0,
                    Color32::from_rgba_unmultiplied(c_r, c_g, c_b, 30),
                );
            }
            painter.rect_filled(
                Rect::from_min_max(pos2(sx0c, wf_rect.top()), pos2(sx1c, wf_rect.bottom())),
                0.0,
                Color32::from_rgba_unmultiplied(c_r, c_g, c_b, 20),
            );
        }
        for (x, e) in [(spx0, (RxId::Sub, false)), (spx1, (RxId::Sub, true))] {
            if rect.x_range().contains(x) {
                let hot = hover_edge == Some(e) || edge == Some(e);
                let w = if hot { 2.0 } else { 1.0 };
                if spec_h > 1.0 {
                    let color = if hot {
                        SUB_COLOR
                    } else {
                        Color32::from_rgba_unmultiplied(c_r, c_g, c_b, 110)
                    };
                    painter.vline(x, spec_rect.y_range(), Stroke::new(w, color));
                }
                painter.vline(
                    x,
                    wf_rect.y_range(),
                    Stroke::new(
                        w,
                        Color32::from_rgba_unmultiplied(c_r, c_g, c_b, if hot { 200 } else { 90 }),
                    ),
                );
            }
        }
        if in_view(sub_hz) {
            let x = view.freq_to_x(sub_hz, &rect);
            // The tuning line thickens under the pointer so the operator can
            // see they have hold of it before they start dragging.
            let w = if hover_sub || sub_drag { 2.5 } else { 1.5 };
            painter.vline(x, spec_rect.y_range(), Stroke::new(w, SUB_COLOR));
            painter.vline(
                x,
                wf_rect.y_range(),
                Stroke::new(w, Color32::from_rgba_unmultiplied(c_r, c_g, c_b, 170)),
            );
            // "SUB" tag on the waterfall, on whichever side of the line has
            // room — at the band edge it would otherwise be clipped away.
            let (anchor, tx) = if x > rect.right() - 34.0 {
                (Align2::RIGHT_TOP, x - 3.0)
            } else {
                (Align2::LEFT_TOP, x + 3.0)
            };
            painter.text(
                pos2(tx, wf_rect.top() + 2.0),
                anchor,
                "SUB",
                FontId::proportional(9.5),
                SUB_COLOR,
            );
        }
    }

    // DXpedition zones: the Fox's half of the passband and the Hound calling
    // zone above it, drawn under the markers so they read as background.
    if dxped != sdroxide_types::DxpedMode::Normal {
        let dial = state.rx_freq_hz();
        let mine = |z: sdroxide_types::DxpedMode| z == dxped;
        for (lo, hi, label, base, ours) in [
            (
                0.0,
                sdroxide_types::FOX_ZONE_MAX_HZ as f64,
                "FOX",
                Color32::from_rgb(255, 120, 160),
                mine(sdroxide_types::DxpedMode::Fox),
            ),
            (
                sdroxide_types::FOX_ZONE_MAX_HZ as f64,
                sdroxide_types::HOUND_ZONE_MAX_HZ as f64,
                "HOUNDS",
                Color32::from_rgb(0, 208, 244),
                mine(sdroxide_types::DxpedMode::Hound),
            ),
        ] {
            let (x0, x1) = (view.freq_to_x(dial + lo, &rect), view.freq_to_x(dial + hi, &rect));
            if x1 <= rect.left() || x0 >= rect.right() {
                continue;
            }
            let band = Rect::from_min_max(
                pos2(x0.max(rect.left()), rect.top()),
                pos2(x1.min(rect.right()), rect.bottom()),
            );
            // The half we transmit in is tinted; the other only outlined, so
            // "where am I allowed to be" is legible at a glance.
            let fill = if ours { 26 } else { 10 };
            painter.rect_filled(
                band,
                0.0,
                Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), fill),
            );
            painter.vline(
                x1,
                rect.y_range(),
                Stroke::new(1.0, Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), 90)),
            );
            if band.width() > 34.0 {
                painter.text(
                    pos2(band.left() + 4.0, rect.top() + 2.0),
                    Align2::LEFT_TOP,
                    label,
                    FontId::proportional(9.5),
                    Color32::from_rgba_unmultiplied(
                        base.r(),
                        base.g(),
                        base.b(),
                        if ours { 220 } else { 110 },
                    ),
                );
            }
        }
    }

    // Digital-mode audio TX marker (cyan) at dial + audio_hz.
    if let Some(a) = digi_audio_hz {
        let hz = state.rx_freq_hz() + a as f64;
        if in_view(hz) {
            let x = view.freq_to_x(hz, &rect);
            painter.vline(x, spec_rect.y_range(), Stroke::new(1.5, crate::theme::CYAN));
            painter.vline(
                x,
                wf_rect.y_range(),
                Stroke::new(1.5, Color32::from_rgba_unmultiplied(0, 208, 244, 160)),
            );
        }
    }
    // Extra tuning lines (RTTY mark/space): thin amber, to aid exact tuning.
    for &a in markers {
        let hz = state.rx_freq_hz() + a as f64;
        if in_view(hz) {
            let x = view.freq_to_x(hz, &rect);
            let c = Color32::from_rgb(255, 190, 90);
            painter.vline(x, spec_rect.y_range(), Stroke::new(1.0, c));
            painter.vline(
                x,
                wf_rect.y_range(),
                Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 190, 90, 150)),
            );
        }
    }
    // Inactive VFO marker — where a split transmit goes, nothing more. The sub
    // receiver used to be pinned here and is drawn separately now that it has a
    // frequency of its own, so this stays quiet: it is a reference line, not a
    // receiver.
    let inactive_hz = match state.active_vfo {
        Vfo::A => state.vfo_b_hz,
        Vfo::B => state.vfo_a_hz,
    };
    if in_view(inactive_hz) {
        let x = view.freq_to_x(inactive_hz, &rect);
        let color = Color32::from_rgba_unmultiplied(255, 170, 40, 90);
        painter.vline(x, spec_rect.y_range(), Stroke::new(1.0, color));
        painter.vline(
            x,
            wf_rect.y_range(),
            Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 170, 40, 110)),
        );
    }

    // --- RIT / XIT incremental-tuning offsets -----------------------------
    // When enabled, show the receive (RIT) and transmit (XIT) offsets from the
    // dial as labelled brackets, with a dashed dial reference (RIT — the red RX
    // marker/passband already sits at dial+RIT) and a TX marker line (XIT).
    if state.rit.enabled || state.xit.enabled {
        let top_y = if spec_h > 1.0 { spec_rect.top() } else { wf_rect.top() } + 4.0;
        let rit_col = Color32::from_rgb(120, 200, 255);
        let xit_col = Color32::from_rgb(120, 230, 140);
        let mut row = 0.0;
        if state.rit.enabled {
            let dial = state.active_freq_hz();
            if in_view(dial) {
                marker_line(
                    &painter,
                    &spec_rect,
                    &wf_rect,
                    spec_h,
                    view.freq_to_x(dial, &rect),
                    Color32::from_gray(150),
                    true,
                );
            }
            offset_bracket(
                &painter,
                rect,
                view.freq_to_x(dial, &rect),
                view.freq_to_x(state.rx_freq_hz(), &rect),
                top_y + row * 16.0,
                rit_col,
                &format!("RIT {:+} Hz", state.rit.hz),
            );
            row += 1.0;
        }
        if state.xit.enabled {
            let tx = state.tx_freq_hz();
            let base = tx - state.xit.effective_hz();
            if in_view(tx) {
                marker_line(
                    &painter,
                    &spec_rect,
                    &wf_rect,
                    spec_h,
                    view.freq_to_x(tx, &rect),
                    xit_col,
                    false,
                );
            }
            offset_bracket(
                &painter,
                rect,
                view.freq_to_x(base, &rect),
                view.freq_to_x(tx, &rect),
                top_y + row * 16.0,
                xit_col,
                &format!("XIT {:+} Hz", state.xit.hz),
            );
        }
    }

    // Skimmer spot boxes over the waterfall (staggered lanes, on top of the
    // markers so they stay readable and clickable). Each box sits to the right
    // of its signal: a faint vertical line marks the signal's centre and a
    // horizontal leader joins it to the box.
    let hover_pos = resp.hover_pos();
    for b in &spot_boxes {
        let spot = &skimmer[b.idx];
        let a = alpha.get(b.idx).copied().unwrap_or(1.0).clamp(0.0, 1.0);
        let hovered = hover_pos.is_some_and(|p| b.rect.contains(p));
        if hovered {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        let border = spot_color(spot, hovered);
        let cy = b.rect.center().y;
        // Vertical indicator over the signal centre (faint, full waterfall).
        let vcol = fade(
            Color32::from_rgba_unmultiplied(
                border.r(),
                border.g(),
                border.b(),
                if hovered { 170 } else { 90 },
            ),
            a,
        );
        painter.vline(b.sig_x, wf_rect.y_range(), Stroke::new(1.0, vcol));
        // Horizontal leader from the signal to the box, with a junction node.
        painter.line_segment(
            [pos2(b.sig_x, cy), pos2(b.rect.left(), cy)],
            Stroke::new(1.0, fade(border, a)),
        );
        painter.circle_filled(pos2(b.sig_x, cy), 1.8, fade(border, a));
        draw_spot_box(&painter, b, spot, hovered, a, digi_audio_hz.is_some());
    }

    // Network spot boxes (bottom-anchored lanes) — DX cluster / POTA / SOTA /
    // PSK. A vertical tick over the signal, a leader up/down to the box.
    for b in &net_boxes {
        let spot = &net_spots[b.idx];
        let a = net_alpha.get(b.idx).copied().unwrap_or(1.0).clamp(0.0, 1.0);
        let hovered = hover_pos.is_some_and(|p| b.rect.contains(p));
        if hovered {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        let border = net_spot_color(spot, hovered);
        let cy = b.rect.center().y;
        let vcol = fade(
            Color32::from_rgba_unmultiplied(
                border.r(),
                border.g(),
                border.b(),
                if hovered { 170 } else { 85 },
            ),
            a,
        );
        painter.vline(b.sig_x, wf_rect.y_range(), Stroke::new(1.0, vcol));
        painter.line_segment(
            [pos2(b.sig_x, cy), pos2(b.rect.left(), cy)],
            Stroke::new(1.0, fade(border, a)),
        );
        painter.circle_filled(pos2(b.sig_x, cy), 1.8, fade(border, a));
        draw_net_box(&painter, b, spot, hovered, a);
    }

    // --- 60-second time gridlines on the waterfall ------------------------
    // The newest row (top of the waterfall, or its bottom when flipped) is
    // "now"; rows away from it are older at `rows_per_sec` rows/second (≈ 1 row
    // per pixel). Draw a faint gray line at each whole UTC minute that falls in
    // the visible window, labelled HH:MM.
    let rows_per_sec = wf.rows_per_sec as f64;
    if rows_per_sec > 0.01 && wf_rect.height() > 4.0 {
        let secs_per_px = 1.0 / rows_per_sec;
        let visible_secs = wf_rect.height() as f64 * secs_per_px;
        let now = wf.now_unix;
        let oldest = now - visible_secs;
        let grid = Color32::from_rgba_unmultiplied(200, 205, 215, 60);
        let mut t = (oldest / 60.0).ceil() * 60.0; // first minute boundary ≥ oldest
        while t <= now {
            let age_px = ((now - t) * rows_per_sec) as f32;
            let y = if view.waterfall_flip {
                wf_rect.bottom() - age_px
            } else {
                wf_rect.top() + age_px
            };
            if (wf_rect.top()..=wf_rect.bottom()).contains(&y) {
                painter.hline(wf_rect.x_range(), y, Stroke::new(1.0, grid));
                let tod = (t as i64).rem_euclid(86_400);
                let text = format!("{:02}:{:02}", tod / 3600, (tod % 3600) / 60);
                label_box(
                    &painter,
                    pos2(wf_rect.left() + 2.0, y + 1.0),
                    &text,
                    Color32::from_gray(215),
                    wf_rect,
                );
            }
            t += 60.0;
        }
    }

    // --- cursor readouts (hover) ------------------------------------------
    // A hovered filter edge shows its offset from its own receiver's centre;
    // otherwise hovering the spectrum/waterfall shows a faint crosshair + the
    // frequency a click would tune to. Suppressed while dragging.
    if let Some(p) = hover_pos {
        if let Some((rx, is_hi)) = hover_edge {
            // Item 7: filter edge offset from the VFO.
            let r = &state.rx[rx.index()];
            let off = if is_hi { r.filter_hi } else { r.filter_lo };
            let edge_x = match (rx, is_hi) {
                (RxId::Main, false) => px0,
                (RxId::Main, true) => px1,
                (RxId::Sub, false) => spx0,
                (RxId::Sub, true) => spx1,
            };
            let ytop = if spec_h > 1.0 { spec_rect.top() } else { wf_rect.top() };
            let tint = if rx == RxId::Main { Color32::from_rgb(255, 190, 120) } else { SUB_COLOR };
            label_box(
                &painter,
                pos2(edge_x + 7.0, ytop + 3.0),
                &format!("{:+} Hz", off.round() as i64),
                tint,
                rect,
            );
        } else if (spec_rect.contains(p) || wf_rect.contains(p))
            && edge.is_none()
            && !resizing
            && !resp.dragged()
            && !spot_boxes.iter().any(|b| b.rect.contains(p))
            && !net_boxes.iter().any(|b| b.rect.contains(p))
        {
            // Item 6: crosshair + click-tune frequency readout.
            let line = Color32::from_rgba_unmultiplied(185, 205, 225, 70);
            if spec_h > 1.0 {
                painter.vline(p.x, spec_rect.y_range(), Stroke::new(1.0, line));
            }
            painter.vline(p.x, wf_rect.y_range(), Stroke::new(1.0, line));
            let text = click_tune_label(view, state, &rect, p.x, digi_audio_hz);
            label_box(&painter, pos2(p.x + 8.0, p.y - 9.0), &text, Color32::WHITE, rect);
        }
    }

    // --- bandwidth measurement (shift+drag) + fade-out --------------------
    // Drawn on top of everything so the markers/label stay readable.
    if let (Some(start_hz), Some(p)) = (measuring, resp.interact_pointer_pos()) {
        // Active drag: full opacity, tracking the pointer.
        let end_hz = view.x_to_freq(p.x, &rect);
        draw_bw_measure(&painter, view, &rect, start_hz, end_hz, 1.0);
    } else if let Some((start_hz, end_hz, t0)) = bw_fade {
        // After release: fade the frozen span out over BW_FADE_SECS, anchored to
        // its frequencies (so it tracks the view if you pan meanwhile).
        let elapsed = ui.input(|i| i.time) - t0;
        if elapsed < BW_FADE_SECS {
            let alpha = (1.0 - elapsed / BW_FADE_SECS) as f32;
            draw_bw_measure(&painter, view, &rect, start_hz, end_hz, alpha);
            ui.ctx().request_repaint(); // keep animating the fade to completion
        } else {
            ui.data_mut(|d| d.insert_temp(fade_id, None::<(f64, f64, f64)>));
        }
    }

    // Chrome: pink cut-corner border + corner accents around the panadapter.
    crate::chrome::paint_cut_border(
        &painter,
        rect.shrink(0.8),
        crate::theme::PINK,
        crate::theme::BG_DEEP,
    );
    crate::chrome::corner_brackets(&painter, rect, crate::theme::PINK);
}

/// How long the released bandwidth measurement lingers while fading out.
const BW_FADE_SECS: f64 = 5.0;

/// Draw the shift+drag bandwidth-measurement overlay: dotted vertical markers at
/// the start and end frequencies, a horizontal span line with end ticks, and the
/// measured bandwidth as a centred label. `alpha` fades the whole thing out.
fn draw_bw_measure(
    p: &egui::Painter,
    view: &ViewState,
    rect: &Rect,
    start_hz: f64,
    end_hz: f64,
    alpha: f32,
) {
    let a = alpha.clamp(0.0, 1.0);
    let base = crate::theme::YELLOW;
    let color = Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), (255.0 * a) as u8);
    let stroke = Stroke::new(1.5, color);

    let x0 = view.freq_to_x(start_hz, rect).clamp(rect.left(), rect.right());
    let x1 = view.freq_to_x(end_hz, rect).clamp(rect.left(), rect.right());
    let bw = (end_hz - start_hz).abs();

    // Dotted vertical markers spanning the whole panadapter (spectrum + scale +
    // waterfall), so the tool works whether or not the spectrum line is shown.
    for x in [x0, x1] {
        p.extend(Shape::dashed_line(
            &[pos2(x, rect.top() + 1.0), pos2(x, rect.bottom() - 1.0)],
            stroke,
            2.5,
            3.5,
        ));
    }
    // Horizontal span line with end ticks.
    let (lo, hi) = (x0.min(x1), x0.max(x1));
    let yl = rect.top() + 26.0;
    p.line_segment([pos2(lo, yl), pos2(hi, yl)], stroke);
    for x in [x0, x1] {
        p.line_segment([pos2(x, yl - 4.0), pos2(x, yl + 4.0)], stroke);
    }
    // Start / end frequency labels at their markers. When the markers are close
    // the two labels would overlap, so push them symmetrically apart (keeping the
    // pair on-screen) so both stay readable.
    let s_text = fmt_mhz(start_hz);
    let e_text = fmt_mhz(end_hz);
    let sw = p.layout_no_wrap(s_text.clone(), FontId::monospace(10.5), color).size().x + 8.0;
    let ew = p.layout_no_wrap(e_text.clone(), FontId::monospace(10.5), color).size().x + 8.0;
    // Order the two labels left→right by marker x.
    let ((lx, lw, lt), (rx, rw, rt)) = if x0 <= x1 {
        ((x0, sw, &s_text), (x1, ew, &e_text))
    } else {
        ((x1, ew, &e_text), (x0, sw, &s_text))
    };
    let (mut cl, mut cr) = (lx, rx);
    let needed = (lw + rw) * 0.5 + 4.0; // min centre distance for a 4px gap
    if cr - cl < needed {
        let mid = (lx + rx) * 0.5;
        cl = mid - needed * 0.5;
        cr = mid + needed * 0.5;
    }
    // Shift the pair as a unit to keep both on-screen without re-overlapping.
    let over_l = rect.left() - (cl - lw * 0.5);
    if over_l > 0.0 {
        cl += over_l;
        cr += over_l;
    }
    let over_r = (cr + rw * 0.5) - rect.right();
    if over_r > 0.0 {
        cl -= over_r;
        cr -= over_r;
    }
    let fy = rect.top() + 4.0;
    faded_label(p, rect, cl, fy, lt, color, a);
    faded_label(p, rect, cr, fy, rt, color, a);
    // Bandwidth centred below the span line.
    faded_label(p, rect, (lo + hi) * 0.5, yl + 6.0, &format_bandwidth(bw), color, a);
}

/// A frequency in Hz as `xx.xxxxx MHz`.
fn fmt_mhz(hz: f64) -> String {
    format!("{:.5} MHz", hz / 1e6)
}

/// Draw `text` in a small box centred on `cx`, top at `y`, clamped to `rect`,
/// with a background whose opacity scales with `a` (for the fade-out).
fn faded_label(
    p: &egui::Painter,
    rect: &Rect,
    cx: f32,
    y: f32,
    text: &str,
    color: Color32,
    a: f32,
) {
    let galley = p.layout_no_wrap(text.to_string(), FontId::monospace(10.5), color);
    let pad = vec2(4.0, 2.0);
    let size = galley.size() + pad * 2.0;
    let bx = (cx - size.x * 0.5).clamp(rect.left(), rect.right() - size.x);
    let by = y.clamp(rect.top(), rect.bottom() - size.y);
    p.rect_filled(
        Rect::from_min_size(pos2(bx, by), size),
        2.0,
        Color32::from_rgba_unmultiplied(0, 0, 0, (180.0 * a) as u8),
    );
    p.galley(pos2(bx + pad.x, by + pad.y), galley, color);
}

/// Format a bandwidth (Hz) as a compact human string.
fn format_bandwidth(hz: f64) -> String {
    if hz >= 10_000.0 {
        format!("{:.1} kHz", hz / 1000.0)
    } else if hz >= 1_000.0 {
        format!("{:.2} kHz", hz / 1000.0)
    } else {
        format!("{:.0} Hz", hz)
    }
}

/// A vertical marker line across the spectrum + waterfall strips (dashed for a
/// reference, solid for a marker); the waterfall copy is faded so it doesn't
/// obscure the trace.
fn marker_line(
    p: &egui::Painter,
    spec: &Rect,
    wf: &Rect,
    spec_h: f32,
    x: f32,
    color: Color32,
    dashed: bool,
) {
    let wf_color = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 150);
    let draw = |a: Pos2, b: Pos2, c: Color32| {
        if dashed {
            p.extend(Shape::dashed_line(&[a, b], Stroke::new(1.0, c), 3.0, 3.0));
        } else {
            p.line_segment([a, b], Stroke::new(1.0, c));
        }
    };
    if spec_h > 1.0 {
        draw(pos2(x, spec.top()), pos2(x, spec.bottom()), color);
    }
    draw(pos2(x, wf.top()), pos2(x, wf.bottom()), wf_color);
}

/// A labelled offset bracket: a horizontal line with end ticks between two x
/// positions, plus the label just past its right end.
fn offset_bracket(
    p: &egui::Painter,
    bounds: Rect,
    xf: f32,
    xt: f32,
    y: f32,
    color: Color32,
    label: &str,
) {
    let xf = xf.clamp(bounds.left(), bounds.right());
    let xt = xt.clamp(bounds.left(), bounds.right());
    let (lo, hi) = (xf.min(xt), xf.max(xt));
    let s = Stroke::new(1.5, color);
    p.line_segment([pos2(lo, y), pos2(hi, y)], s);
    p.line_segment([pos2(xf, y - 3.0), pos2(xf, y + 3.0)], s);
    p.line_segment([pos2(xt, y - 3.0), pos2(xt, y + 3.0)], s);
    label_box(p, pos2(hi + 4.0, y - 6.0), label, color, bounds);
}

/// Draw `text` in a small semi-transparent black box, clamped inside `bounds`.
fn label_box(p: &egui::Painter, top_left: Pos2, text: &str, fg: Color32, bounds: Rect) {
    let galley = p.layout_no_wrap(text.to_string(), FontId::monospace(11.0), fg);
    let pad = vec2(4.0, 2.0);
    let size = galley.size() + pad * 2.0;
    let x = (top_left.x).min(bounds.right() - size.x).max(bounds.left());
    let y = (top_left.y).clamp(bounds.top(), bounds.bottom() - size.y);
    let bg = Rect::from_min_size(pos2(x, y), size);
    p.rect_filled(bg, 2.0, Color32::from_rgba_unmultiplied(0, 0, 0, 180));
    p.galley(pos2(x + pad.x, y + pad.y), galley, fg);
}

/// The value a left-click at screen `x` would set: the audio TX offset in a
/// digital mode, else the (10 Hz-rounded) dial frequency.
fn click_tune_label(
    view: &ViewState,
    state: &RadioState,
    rect: &Rect,
    x: f32,
    digi_audio_hz: Option<f32>,
) -> String {
    let hz = view.x_to_freq(x, rect);
    if digi_audio_hz.is_some() {
        let audio = (hz - state.rx_freq_hz()).clamp(200.0, 3500.0);
        format!("{audio:.0} Hz")
    } else {
        let tuned = (hz / CLICK_TUNE_STEP).round() * CLICK_TUNE_STEP;
        format!("{:.5} MHz", tuned / 1e6)
    }
}

/// Per-pixel polyline of the frame's bins (or of `values` when given, e.g.
/// the peak-hold bins) mapped through the current viewport. This is the
/// expensive path the [`TraceCache`] avoids on unchanged repaints.
fn compute_trace(
    view: &ViewState,
    f: &SpectrumFrame,
    values: Option<&[f32]>,
    rect: &Rect,
) -> Vec<Pos2> {
    let n = values.map(|v| v.len()).unwrap_or(f.bins.len());
    if n == 0 {
        return Vec::new();
    }
    let base = f.center_hz - f.span_hz / 2.0;
    let w = rect.width().max(1.0) as usize;
    let mut points = Vec::with_capacity(w);
    for px in 0..w {
        let x = rect.left() + px as f32;
        let hz = view.x_to_freq(x, rect);
        let bin_f = (hz - base) / f.span_hz * n as f64;
        let v = if (0.0..n as f64).contains(&bin_f) {
            let i = bin_f as usize;
            let raw = match values {
                Some(v) => v[i],
                None => f.bins[i] as f32,
            };
            raw / 255.0
        } else {
            0.0
        };
        points.push(Pos2::new(x, rect.bottom() - v * rect.height()));
    }
    points
}

fn draw_grid(painter: &egui::Painter, view: &ViewState, rect: &Rect) {
    let grid = Stroke::new(0.5, Color32::from_gray(42));
    // dB lines every 20 dB of the display range.
    let range = view.db_ceil - view.db_floor;
    if range > 1.0 {
        let mut db = (view.db_floor / 20.0).ceil() * 20.0;
        while db < view.db_ceil {
            let frac = (db - view.db_floor) / range;
            let y = rect.bottom() - frac * rect.height();
            painter.hline(rect.x_range(), y, grid);
            painter.text(
                pos2(rect.left() + 2.0, y - 1.0),
                Align2::LEFT_BOTTOM,
                format!("{db:.0}"),
                FontId::monospace(9.0),
                Color32::from_gray(110),
            );
            db += 20.0;
        }
    }
    for hz in freq_gridlines(view) {
        let x = view.freq_to_x(hz, rect);
        painter.vline(x, rect.y_range(), grid);
    }
}

fn draw_scale(painter: &egui::Painter, view: &ViewState, rect: &Rect) {
    painter.rect_filled(*rect, 0.0, Color32::from_gray(20));
    for hz in freq_gridlines(view) {
        let x = view.freq_to_x(hz, rect);
        painter.vline(
            x,
            egui::Rangef::new(rect.top(), rect.top() + 5.0),
            Stroke::new(1.0, Color32::from_gray(120)),
        );
        painter.text(
            pos2(x, rect.center().y + 2.0),
            Align2::CENTER_CENTER,
            format!("{:.4}", hz / 1e6),
            FontId::monospace(10.0),
            Color32::from_gray(190),
        );
    }
}

/// Gridline frequencies at a 1/2/5·10^k step giving ~5–10 lines.
fn freq_gridlines(view: &ViewState) -> Vec<f64> {
    let span = view.span();
    let raw = span / 8.0;
    let mag = 10f64.powf(raw.log10().floor());
    let step =
        [1.0, 2.0, 5.0, 10.0].iter().map(|m| m * mag).find(|&s| s >= raw).unwrap_or(mag * 10.0);
    let mut out = Vec::new();
    let mut hz = (view.view_lo_hz / step).ceil() * step;
    while hz <= view.view_hi_hz {
        out.push(hz);
        hz += step;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How long a dial released at `v0` points/second keeps turning, and how far
    /// it travels, stepped at a plausible frame rate.
    fn coast(v0: f32) -> (f32, f32) {
        let dt = 1.0 / 60.0;
        let (mut vx, mut t, mut px) = (v0, 0.0, 0.0);
        while vx.abs() >= FLING_STOP {
            vx = fling_decay(vx, dt);
            px += vx * dt;
            t += dt;
            assert!(t < 60.0, "a coast from {v0} px/s never stopped");
        }
        (t, px)
    }

    /// The point of the whole thing: a faster flick spins the dial for longer
    /// and carries it further, the way a weighted VFO knob does.
    #[test]
    fn a_faster_flick_coasts_longer_and_further() {
        let (slow_t, slow_px) = coast(400.0);
        let (mid_t, mid_px) = coast(1500.0);
        let (fast_t, fast_px) = coast(4000.0);
        assert!(slow_t < mid_t && mid_t < fast_t, "{slow_t} {mid_t} {fast_t}");
        assert!(slow_px < mid_px && mid_px < fast_px, "{slow_px} {mid_px} {fast_px}");
        // And it is a dial, not a slingshot: even a hard flick settles in a
        // couple of seconds rather than drifting across the band.
        assert!(fast_t < 3.5, "a hard flick should still settle promptly, took {fast_t}s");
    }

    /// A coast only ever runs down. Reversing direction would read as the dial
    /// bouncing off something, and at a long frame the naive decay does exactly
    /// that.
    #[test]
    fn a_coast_never_spins_backwards() {
        for v0 in [-4000.0f32, -300.0, 300.0, 4000.0] {
            let mut vx = v0;
            for _ in 0..200 {
                // Deliberately long frames: the worst case the clamp allows.
                let next = fling_decay(vx, 0.1);
                assert!(next == 0.0 || next.signum() == v0.signum(), "{v0} reversed to {next}");
                assert!(next.abs() <= vx.abs(), "{v0} sped up: {vx} -> {next}");
                vx = next;
            }
            assert_eq!(vx, 0.0, "a coast from {v0} px/s should have run out");
        }
    }

    /// A slow, careful drag must place the dial exactly where it is released —
    /// the launch threshold is what keeps precise tuning precise.
    #[test]
    fn a_careful_drag_does_not_fling() {
        // A release just under the threshold is suppressed, and the coast it
        // would have had is short enough that nothing visible is lost — which is
        // what makes the threshold safe to set where it is.
        let (t, px) = coast(FLING_MIN_LAUNCH);
        assert!(t < 0.35, "the shortest coast that can launch is {t}s — too long to feel bounded");
        assert!(px < 30.0, "suppressing it costs {px} points of travel");
    }
}
