//! The egui chrome drawn inside the solar-system window: a wrapping top bar of
//! captioned modules over the 3D scene, styled with the app's own
//! [`crate::chrome`] widgets so the second window reads as part of sdroxide.

use eframe::egui::{self, RichText};
use sdroxide_solar::{SdoChannel, SolarData, Source, timefmt};

use super::state::{Focus, SolarUi};
use crate::chrome;
use crate::theme;
use crate::view::solar_layer as layer;

/// Layer chips, in bar order.
///
/// The star field and the heliographic graticule are not among them: they are
/// the backdrop and the coordinate frame everything else is read against, so
/// they are always drawn rather than being switches.
const LAYERS: [(u32, &str, &str); 10] = [
    (layer::ORBITS, "ORBITS", "Orbital paths"),
    (
        layer::CLOUDS,
        "CLOUDS",
        "Cloud cover from NOAA's global geostationary mosaic, drawn as a depth of air \
         rather than a picture stuck on the surface: infrared says how cold each cloud \
         top is, and that is how high it stands. Thunderstorms flash inside the towers \
         they build.",
    ),
    (
        layer::PLANETS,
        "PLANETS",
        "The other seven planets, eighteen of their moons, and Saturn's and Uranus's rings. \
         Planet positions come from JPL's Keplerian element set; the moons are circular orbits \
         fitted to JPL Horizons.",
    ),
    (layer::CME, "CME", "Coronal mass ejection trajectory cones"),
    (
        layer::SUN_OBS,
        "SUN OBS",
        "Solar observations on the Sun's disk: sunspot active regions, and where the \
         flares came from",
    ),
    (layer::LABELS, "LABELS", "Body and region labels"),
    (layer::QSO, "QSO", "Decoded FT8/FT4 stations and the path to the station being worked"),
    (layer::SATS, "SATS", "Amateur-radio satellites, their orbits and elevation from your QTH"),
    (
        layer::AURORA,
        "AURORA",
        "The auroral oval from NOAA's OVATION model, drawn as emission shells at their real \
         altitudes, with the equatorward edge of the 10 % contour marked on the surface",
    ),
    (
        layer::AWARDS,
        "AWARDS",
        "Award coverage: every DXCC entity painted by what your logbook is still missing. \
         Orange burns where you have never worked the entity, amber where you have but it is \
         unconfirmed, dim green where a QSL has come back. Follows the band filter in the \
         AWARDS window.",
    ),
];

/// How often the view redraws when nothing is happening to it.
///
/// This is a clock, not a document: the Earth turns, the terminator moves, the
/// Moon goes round and the QSO arcs follow the contacts. It has to keep running
/// whether or not the pointer is over it and whether or not the window has
/// focus — a frozen orrery in the corner of the screen is worse than none.
const IDLE_FRAME: std::time::Duration = std::time::Duration::from_millis(33);

pub fn ui(ui: &mut egui::Ui, st: &mut SolarUi) {
    ui.ctx().request_repaint_after(IDLE_FRAME);
    // Take a snapshot of the feed's data for this frame. Cloning the `Arc`
    // first means the guard's lifetime is not tied to `st`, which is borrowed
    // mutably by every module below.
    let handle = st.data.clone();
    let guard = handle.as_ref().map(|d| d.lock().unwrap_or_else(|e| e.into_inner()));
    let data = guard.as_deref();
    let now = super::wall_clock_unix() as i64;

    egui::Panel::top(egui::Id::new("solar-top"))
        .frame(egui::Frame::new().fill(theme::BG_DEEP).inner_margin(egui::Margin::symmetric(8, 6)))
        .show(ui, |ui| {
            chrome::angled_frame(ui, theme::PINK, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                ui.with_layout(
                    egui::Layout::left_to_right(egui::Align::Min).with_main_wrap(true),
                    |ui| {
                        view_module(ui, st);
                        layers_module(ui, st);
                        sun_module(ui, st, data, now);
                        scale_module(ui, st);
                        time_module(ui, st);
                        activity_module(ui, st);
                    },
                );
            });
        });

    scene(ui, st, data);
}

/// Camera target + the animated tour toggle.
fn view_module(ui: &mut egui::Ui, st: &mut SolarUi) {
    chrome::module(ui, "View", 336.0, |ui| {
        target_button(ui, st);
        if chrome::chip_accent(ui, st.view.auto, "▶ AUTO", theme::CYAN, theme::INK_ON_CYAN)
            .on_hover_text(
                "Fly a spline through a set of framed viewpoints. Any mouse input cancels it.",
            )
            .clicked()
        {
            st.view.auto = !st.view.auto;
            if st.view.auto {
                st.tour.request_resume();
            }
        }
    });
}

/// The camera target, as a button that opens the whole solar system.
///
/// A chip per body would be thirty chips across the top bar, so the current
/// target is the button face and the rest live in a popup laid out the way the
/// system is: the Sun and the Earth–Moon pair first, then one row per planet
/// with its own moons beside it.
fn target_button(ui: &mut egui::Ui, st: &mut SolarUi) {
    let btn = chrome::chip(ui, true, RichText::new(format!("◎ {}", st.focus().short())).size(13.0))
        .on_hover_text(
            "Body the camera orbits. Planets, moons and their labels can also be clicked \
             directly in the view.",
        );

    let mut chosen = None;
    let resp = egui::Popup::from_toggle_button_response(&btn)
        .frame(chrome::window_frame())
        .close_behavior(egui::PopupCloseBehavior::CloseOnClick)
        .show(|ui| {
            ui.set_max_width(560.0);
            for (caption, targets) in Focus::groups() {
                ui.label(
                    RichText::new(caption.to_uppercase()).color(theme::CYAN_DIM).size(9.5).strong(),
                );
                ui.horizontal_wrapped(|ui| {
                    for f in targets {
                        // Moons are dimmer, so a row reads as "this planet, and
                        // the things that go round it".
                        let text = if f.is_satellite() {
                            RichText::new(f.short()).size(11.5).color(theme::CYAN_DIM)
                        } else {
                            RichText::new(f.short()).size(13.0)
                        };
                        if chrome::chip(ui, st.focus() == f, text).clicked() {
                            chosen = Some(f);
                        }
                    }
                });
            }
            ui.add_space(2.0);
            ui.label(
                RichText::new(
                    "Moons ride circular orbits fitted to JPL Horizons: within a degree or two \
                     of where they really are, and up to six for Miranda, whose orbit plane \
                     swings too fast for a circle to follow.",
                )
                .color(theme::LINE_LIT)
                .size(10.0),
            );
        });
    if let Some(r) = &resp {
        chrome::paint_popup_cut_border(ui.ctx(), &r.response, 1.0);
    }
    if let Some(f) = chosen {
        st.set_focus(f);
    }
}

fn layers_module(ui: &mut egui::Ui, st: &mut SolarUi) {
    // Wide enough for the nine chips that show without a logbook. AWARDS, which
    // only appears once there is a log to colour, spills past the edge — it
    // always has.
    chrome::module(ui, "Layers", 610.0, |ui| {
        for (bit, label, hint) in LAYERS {
            // The award layer has nothing to paint without a logbook to paint
            // it from — the browser tab has none — and a chip that provably
            // does nothing is worse than no chip.
            if bit == layer::AWARDS && st.awards.is_empty() {
                continue;
            }
            // `layer` is already "any of these bits", so a chip standing for a
            // pair lights when either half is on and clears both when clicked.
            let on = st.layer(bit);
            if chrome::chip(ui, on, label).on_hover_text(hint).clicked() {
                st.set_layers(bit, !on);
            }
        }
    });
}

/// Which SDO product wraps the Sun, plus the honest freshness readout.
fn sun_module(ui: &mut egui::Ui, st: &mut SolarUi, data: Option<&SolarData>, now: i64) {
    chrome::module(ui, "Sun", 470.0, |ui| {
        let current = SdoChannel::from_u8(st.view.channel);
        for c in SdoChannel::ALL {
            if chrome::chip(ui, current == c, c.label()).on_hover_text(c.description()).clicked() {
                st.view.channel = c.to_u8();
            }
        }
        if chrome::chip(ui, false, "↻").on_hover_text("Fetch everything again now").clicked() {
            st.refresh_requested = true;
        }
        if chrome::chip(ui, st.view.all_satellites, "ALL SATS")
            .on_hover_text(
                "Show every satellite in the element set, not just the popular ones. \
                 Orbit rings stay on the curated few — ninety at once is unreadable.",
            )
            .clicked()
        {
            st.view.all_satellites = !st.view.all_satellites;
        }

        // Say what is actually being shown. Presenting hours-old cached data as
        // if it were current is the one thing this readout must not do.
        let (text, color) = match data {
            None => ("starting…".to_string(), theme::CYAN_DIM),
            Some(d) => {
                let s = d.status(Source::Sun);
                match (s.age_secs(now), &s.last_error) {
                    (Some(age), None) => (timefmt::age(age), theme::GREEN),
                    (Some(age), Some(_)) => {
                        (format!("{} · offline", timefmt::age(age)), theme::YELLOW)
                    }
                    (None, Some(_)) => ("offline".to_string(), theme::PINK),
                    (None, None) => ("…".to_string(), theme::CYAN_DIM),
                }
            }
        };
        ui.label(RichText::new(text).color(color).size(10.5));
    });
}

/// Size exaggeration. Positions are always real; only radii (and optionally the
/// Moon's orbit) are scaled, or nothing at this distance would be visible.
fn scale_module(ui: &mut egui::Ui, st: &mut SolarUi) {
    chrome::module(ui, "Scale", 400.0, |ui| {
        // The Moon renders *inside* the Earth once the exaggerated radii exceed
        // the (unexaggerated) Earth–Moon distance, so cap body scale against it.
        let max_body = super::max_body_scale(st.view.moon_orbit_scale);
        ui.label(RichText::new("body").color(theme::CYAN_DIM).size(10.0));
        ui.add(
            egui::DragValue::new(&mut st.view.body_scale)
                .speed(0.25)
                .range(1.0..=max_body as f64)
                .suffix("×"),
        )
        .on_hover_text(format!(
            "Earth/Moon radius exaggeration (max {max_body:.0}× at this moon-orbit scale)"
        ));
        ui.label(RichText::new("moon orbit").color(theme::CYAN_DIM).size(10.0));
        ui.add(
            egui::DragValue::new(&mut st.view.moon_orbit_scale)
                .speed(0.1)
                .range(1.0..=30.0)
                .suffix("×"),
        )
        .on_hover_text("Stretch the Earth→Moon distance so the pair can be seen apart");
        ui.label(RichText::new("sun").color(theme::CYAN_DIM).size(10.0));
        ui.add(
            egui::DragValue::new(&mut st.view.sun_scale).speed(0.1).range(1.0..=20.0).suffix("×"),
        )
        .on_hover_text(
            "Sun radius exaggeration. Leave at 1× to keep the CME geometry readable — \
             a swollen Sun swallows the base of every cone.",
        );
        st.view.body_scale = st.view.body_scale.clamp(1.0, max_body);
    });
}

/// Scrub the whole scene forward and back in time.
fn time_module(ui: &mut egui::Ui, st: &mut SolarUi) {
    chrome::module(ui, "Time", 300.0, |ui| {
        if chrome::chip(ui, st.sim_offset_s == 0.0, "NOW").clicked() {
            st.sim_offset_s = 0.0;
        }
        for (label, dt) in
            [("−24h", -86400.0), ("−1h", -3600.0), ("+1h", 3600.0), ("+24h", 86400.0)]
        {
            if chrome::chip(ui, false, label).clicked() {
                st.sim_offset_s += dt;
            }
        }
    });
}

/// The FT8/FT4 activity time-lapse: where in the last hour the globe's arcs
/// are being replayed from, how long a trail they leave, and how fast the
/// replay runs.
fn activity_module(ui: &mut egui::Ui, st: &mut SolarUi) {
    chrome::module(ui, "Activity", 430.0, |ui| {
        let hour = crate::digi_map::HISTORY_S as f64;
        if chrome::chip(ui, st.lapse_live() && !st.lapse_playing, "LIVE")
            .on_hover_text("Follow the band as it happens")
            .clicked()
        {
            st.lapse_playing = false;
            st.set_lapse_back(0.0);
        }
        if chrome::chip_accent(
            ui,
            st.lapse_playing,
            if st.lapse_playing { "⏸ REPLAY" } else { "▶ REPLAY" },
            theme::CYAN,
            theme::INK_ON_CYAN,
        )
        .on_hover_text("Replay the last hour of decodes, over and over")
        .clicked()
        {
            st.lapse_playing = !st.lapse_playing;
            // Starting from the present would replay nothing, so a replay
            // begun while live starts at the top of the hour.
            if st.lapse_playing && st.lapse_live() {
                st.set_lapse_back(hour);
            }
        }

        // The head, as minutes ago. Dragging it is also how you stop the
        // replay running away from where you wanted to look.
        let mut back_min = (st.lapse_back_s / 60.0) as f32;
        let resp = ui.add(
            egui::DragValue::new(&mut back_min)
                .speed(0.5)
                .range(0.0..=(hour / 60.0))
                .suffix(" min ago"),
        );
        if resp.on_hover_text("Where in the last hour the globe is showing").changed() {
            st.set_lapse_back(back_min as f64 * 60.0);
            st.lapse_playing = false;
        }

        ui.label(RichText::new("trail").color(theme::CYAN_DIM).size(10.0));
        ui.add(
            egui::DragValue::new(&mut st.view.lapse_trail_min)
                .speed(0.25)
                .range(0.5..=(hour / 60.0))
                .suffix(" min"),
        )
        .on_hover_text("How long a decode's arc stays on the globe behind the head");

        ui.label(RichText::new("speed").color(theme::CYAN_DIM).size(10.0));
        ui.add(
            egui::DragValue::new(&mut st.view.lapse_speed)
                .speed(1.0)
                .range(1.0..=600.0)
                .suffix("×"),
        )
        .on_hover_text("How much faster than real time the replay runs");

        let hits = st.digi.history.len();
        let (text, color) = if !st.layer(layer::QSO) {
            ("QSO layer off".to_string(), theme::YELLOW)
        } else if hits == 0 {
            ("no decodes yet".to_string(), theme::CYAN_DIM)
        } else if st.lapse_live() {
            (format!("{hits} in the hour"), theme::GREEN)
        } else {
            (format!("−{:.0} min", st.lapse_back_s / 60.0), theme::CYAN)
        };
        ui.label(RichText::new(text).color(color).size(10.5));
    });
}

/// Step the activity replay head, at `speed` × real time, looping round the
/// hour. Looping and not stopping at the present on purpose: the point of a
/// replay is to watch the opening come and go more than once.
fn advance_lapse(ui: &egui::Ui, st: &mut SolarUi, dt: f32) {
    if !st.lapse_playing {
        return;
    }
    let step = dt.max(0.0) as f64 * st.view.lapse_speed.clamp(1.0, 600.0) as f64;
    let back = st.lapse_back_s - step;
    st.set_lapse_back(if back <= 0.0 { crate::digi_map::HISTORY_S as f64 } else { back });
    ui.ctx().request_repaint();
}

/// The 3D scene: mouse interaction, the wgpu paint callback, then the readouts
/// painted over it.
fn scene(ui: &mut egui::Ui, st: &mut SolarUi, data: Option<&SolarData>) {
    let rect = ui.available_rect_before_wrap();
    if rect.width() < 4.0 || rect.height() < 4.0 {
        return;
    }
    let resp = ui.allocate_rect(rect, egui::Sense::click_and_drag());
    interact(ui, st, &resp);

    let ppp = ui.ctx().pixels_per_point();
    let px = [(rect.width() * ppp).round().max(1.0), (rect.height() * ppp).round().max(1.0)];
    let sim_now = super::wall_clock_unix() + st.sim_offset_s;
    let anim = ui.input(|i| i.time);
    // One elapsed-time step for everything animated off the wall clock, so the
    // tour and the activity replay cannot disagree about how long a frame was.
    let dt =
        if st.last_frame_time <= 0.0 { 1.0 / 60.0 } else { (anim - st.last_frame_time) as f32 };
    st.last_frame_time = anim;
    advance_tour(ui, st, sim_now, dt);
    advance_lapse(ui, st, dt);
    reframe(st, sim_now);
    let mut scene = super::scene::build(st, data, sim_now, px, anim as f32);
    // Labels and pick targets are egui's business, not the GPU's, so they come
    // out before the rest of the scene is handed to the paint callback.
    let labels = std::mem::take(&mut scene.labels);
    let picks = std::mem::take(&mut scene.picks);
    let view_proj = scene.globals.view_proj;

    let (sun_img, sun_gen, aurora, aurora_gen, clouds, clouds_gen) = match data {
        Some(d) => (
            d.sun.clone(),
            d.sun_gen,
            d.aurora.clone(),
            d.aurora_gen,
            d.clouds.clone(),
            d.clouds_gen,
        ),
        None => (None, 0, None, 0, None, 0),
    };
    ui.painter().add(crate::egui_wgpu::Callback::new_paint_callback(
        rect,
        super::gpu::SolarCallback {
            scene: std::sync::Arc::new(scene),
            px_size: [px[0] as u32, px[1] as u32],
            sun_img,
            sun_gen,
            aurora,
            aurora_gen,
            clouds,
            clouds_gen,
        },
    ));

    let took_click = draw_labels(ui, st, rect, &view_proj, &labels, &resp);
    // Only if a label did not already take it: a name sits on top of its own
    // body, and clicking the text should not be ambiguous.
    pick_bodies(ui, st, rect, &view_proj, &picks, &resp, took_click);
    let clock_rect = clock(ui, rect, sim_now, st.sim_offset_s != 0.0);
    sat_search(ui, st, data, rect, clock_rect);
    let below = weather_panel(ui, st, data, rect, sim_now as i64);
    aurora_panel(ui, st, data, rect, below, sim_now as i64);
    info_card(ui, st, data, rect, sim_now);
    award_panel(ui, st, rect);
    clouds_note(ui, st, data, rect, sim_now as i64);
    impact_banner(ui, data, rect, sim_now as i64);
    pass_window(ui, st, data, sim_now);

    if st.qth.is_none() {
        ui.painter().text(
            rect.right_top() + egui::vec2(-12.0, 12.0),
            egui::Align2::RIGHT_TOP,
            "QTH not set — enter your grid square in Settings",
            egui::FontId::proportional(12.5),
            theme::YELLOW,
        );
    }
}

/// Project a world point into the widget. `None` when it is behind the eye.
fn project(view_proj: &[[f32; 4]; 4], rect: egui::Rect, world: [f32; 3]) -> Option<egui::Pos2> {
    // Column-major, so column i scales input component i.
    let mut o = [0.0f32; 4];
    for (r, out) in o.iter_mut().enumerate() {
        *out = view_proj[0][r] * world[0]
            + view_proj[1][r] * world[1]
            + view_proj[2][r] * world[2]
            + view_proj[3][r];
    }
    if o[3] <= 0.0 {
        return None;
    }
    Some(egui::pos2(
        rect.left() + (o[0] / o[3] * 0.5 + 0.5) * rect.width(),
        rect.top() + (0.5 - o[1] / o[3] * 0.5) * rect.height(),
    ))
}

/// Project the scene's labels to screen space, paint them, and let one be
/// clicked. Returns whether the click was consumed.
///
/// The 3D pass has no text rendering, and adding one for a dozen short strings
/// would be far more machinery than projecting a dozen points with the same
/// matrix the vertex shaders use.
fn draw_labels(
    ui: &egui::Ui,
    st: &mut SolarUi,
    rect: egui::Rect,
    view_proj: &[[f32; 4]; 4],
    labels: &[super::scene::Label],
    resp: &egui::Response,
) -> bool {
    use super::scene::Click;

    let font = egui::FontId::proportional(11.5);
    let pointer = ui.input(|i| i.pointer.interact_pos());
    // A press-and-release without dragging: a drag is the camera's, a click is
    // the label's.
    let clicked = resp.clicked();
    let mut hit: Option<Click> = None;
    let p = ui.painter();

    for l in labels {
        let Some(anchor) = project(view_proj, rect, l.world) else { continue };
        let pos = anchor + egui::vec2(l.offset[0], l.offset[1]);
        if !rect.contains(pos) {
            continue;
        }

        let galley = p.layout_no_wrap(l.text.clone(), font.clone(), l.color);
        let text_rect =
            egui::Rect::from_min_size(pos - egui::vec2(0.0, galley.size().y * 0.5), galley.size());
        let hovered =
            l.click != Click::None && pointer.is_some_and(|q| text_rect.expand(4.0).contains(q));
        if hovered {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            // A backing plate, so it reads as something you can press.
            p.rect_filled(
                text_rect.expand2(egui::vec2(4.0, 2.0)),
                0,
                theme::FILL.gamma_multiply(0.85),
            );
            if clicked {
                hit = Some(l.click);
            }
        }

        // A dark halo, so a label stays readable over the bright solar disk as
        // well as over empty space.
        let color = if hovered { theme::TEXT_STRONG } else { l.color };
        let shadow = egui::Color32::from_black_alpha(color.a().saturating_sub(40));
        p.text(
            pos + egui::vec2(1.0, 1.0),
            egui::Align2::LEFT_CENTER,
            &l.text,
            font.clone(),
            shadow,
        );
        p.text(pos, egui::Align2::LEFT_CENTER, &l.text, font.clone(), color);
    }

    match hit {
        // Clicking the open satellite's label again closes the table.
        Some(Click::Sat(id)) => {
            st.selected_sat = if st.selected_sat == Some(id) { None } else { Some(id) };
            true
        }
        Some(Click::Focus(f)) => {
            st.set_focus(f);
            true
        }
        _ => false,
    }
}

/// Let the bodies themselves be clicked, not only their names.
///
/// Picking is done in screen space against the projected centres rather than by
/// reading back a depth or id buffer: there are under thirty candidates, and a
/// GPU readback would stall the frame for something a dot product answers.
fn pick_bodies(
    ui: &egui::Ui,
    st: &mut SolarUi,
    rect: egui::Rect,
    view_proj: &[[f32; 4]; 4],
    picks: &[super::scene::Pick],
    resp: &egui::Response,
    consumed: bool,
) {
    let Some(pointer) = ui.input(|i| i.pointer.interact_pos()) else { return };
    if !rect.contains(pointer) {
        return;
    }

    // Nearest hit wins, measured in fractions of each body's own radius, so a
    // moon in front of its planet is reachable rather than swallowed by it.
    let mut best: Option<(f32, &super::scene::Pick, egui::Pos2)> = None;
    for pick in picks {
        if st.focus() == pick.focus {
            continue; // already the target; nothing to click it for
        }
        let Some(pos) = project(view_proj, rect, pick.world) else { continue };
        let d = pos.distance(pointer);
        if d > pick.radius_px {
            continue;
        }
        let score = d / pick.radius_px;
        if best.as_ref().is_none_or(|(b, _, _)| score < *b) {
            best = Some((score, pick, pos));
        }
    }
    let Some((_, pick, pos)) = best else { return };

    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    // A reticle, so it is obvious what the click will grab.
    let r = pick.radius_px.clamp(9.0, 90.0);
    ui.painter().circle_stroke(pos, r, egui::Stroke::new(1.2, theme::CYAN.gamma_multiply(0.8)));
    ui.painter().text(
        pos + egui::vec2(0.0, -r - 4.0),
        egui::Align2::CENTER_BOTTOM,
        pick.focus.short(),
        egui::FontId::proportional(11.0),
        theme::CYAN,
    );
    if resp.clicked() && !consumed {
        st.set_focus(pick.focus);
    }
}

/// Frame the camera on a newly chosen target.
///
/// Without this, picking Io while framed on the whole system leaves the camera
/// 2 AU away pointed at something 3600 km across, which reads as the click
/// having done nothing. The distance is set from the target's own radius, and
/// the orientation is left alone so the move feels like a pan rather than a
/// reset.
fn reframe(st: &mut SolarUi, sim_now: f64) {
    if !std::mem::take(&mut st.retarget) {
        return;
    }
    let b = super::scene::bodies(st, sim_now);
    let (_, radius) = b.focus(st.focus());
    let (lo, hi) = super::camera::dist_range(radius);
    st.view.dist = (radius * 9.0).clamp(lo, hi);
}

/// The pass table for the selected satellite.
///
/// Prediction is expensive — stepping a whole orbit and bisecting each horizon
/// crossing — so it is computed once per selection and refreshed only as it
/// ages out, never per frame.
fn pass_window(ui: &egui::Ui, st: &mut SolarUi, data: Option<&SolarData>, sim_now: f64) {
    use sdroxide_solar::{PassSearch, satellites::compass};

    let Some(id) = st.selected_sat else { return };
    let Some(d) = data else { return };
    let Some((lat, lon)) = st.qth else {
        st.selected_sat = None;
        return;
    };
    let Some(sat) = d.satellites().find(|s| s.norad_id == id) else {
        // The element set was replaced and no longer has it.
        st.selected_sat = None;
        return;
    };

    // Recompute on a change of satellite or QTH, or once the prediction has
    // aged enough that its first pass may have gone by.
    let stale = st.sat_passes.as_ref().is_none_or(|c| {
        c.norad_id != id || c.qth != (lat, lon) || (sim_now - c.computed_unix).abs() > 300.0
    });
    if stale {
        st.sat_passes = Some(super::state::SatPasses {
            norad_id: id,
            name: sat.name.clone(),
            qth: (lat, lon),
            computed_unix: sim_now,
            // Two days ahead is enough for "the next few" without spending long
            // in the search: a LEO satellite gives four to six usable passes.
            result: sat.next_passes(lat, lon, sim_now, 48.0 * 3600.0, 6),
        });
    }
    let Some(cache) = &st.sat_passes else { return };

    let mut open = true;
    egui::Window::new(format!("{}  ·  PASSES FROM {}", cache.name, st.qth_grid))
        .id(egui::Id::new("solar-passes"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .frame(chrome::window_frame())
        .default_pos(ui.max_rect().center() - egui::vec2(230.0, 120.0))
        .show(ui.ctx(), |ui| {
            ui.label(
                RichText::new(format!(
                    "elements {} old · SGP4",
                    timefmt::age(sat.element_age_s(sim_now) as i64)
                ))
                .color(theme::CYAN_DIM)
                .size(10.0),
            );
            ui.add_space(4.0);

            match &cache.result {
                PassSearch::AlwaysVisible { elevation, azimuth } => {
                    ui.label(
                        RichText::new("Geostationary — always above your horizon.")
                            .color(theme::GREEN)
                            .size(12.5),
                    );
                    ui.label(
                        RichText::new(format!(
                            "Point at {azimuth:.0}° ({}), elevation {elevation:.0}°.",
                            compass(*azimuth)
                        ))
                        .color(theme::TEXT),
                    );
                }
                PassSearch::NeverVisible => {
                    ui.label(
                        RichText::new("No passes in the next 48 hours from your QTH.")
                            .color(theme::YELLOW),
                    );
                }
                PassSearch::Passes(passes) => {
                    egui::Grid::new("solar-pass-grid").num_columns(6).spacing([14.0, 3.0]).show(
                        ui,
                        |ui| {
                            for h in ["START", "END", "DUR", "AOS", "LOS", "MAX EL"] {
                                ui.label(
                                    RichText::new(h).color(theme::CYAN_DIM).size(9.5).strong(),
                                );
                            }
                            ui.end_row();
                            for p in passes {
                                let soon = p.rise_unix as f64 - sim_now;
                                // The one happening now, or next, is the one the
                                // operator cares about.
                                let color = if soon < 0.0 {
                                    theme::GREEN
                                } else if soon < 3600.0 {
                                    theme::YELLOW
                                } else {
                                    theme::TEXT
                                };
                                ui.label(RichText::new(timefmt::ymd_hm(p.rise_unix)).color(color));
                                ui.label(RichText::new(hhmm(p.set_unix)).color(color));
                                ui.label(
                                    RichText::new(format!("{} min", p.duration_s() / 60))
                                        .color(color),
                                );
                                ui.label(
                                    RichText::new(format!(
                                        "{:.0}° {}",
                                        p.rise_az,
                                        compass(p.rise_az)
                                    ))
                                    .color(color),
                                );
                                ui.label(
                                    RichText::new(format!(
                                        "{:.0}° {}",
                                        p.set_az,
                                        compass(p.set_az)
                                    ))
                                    .color(color),
                                );
                                ui.label(
                                    RichText::new(format!("{:.0}°  {}", p.max_el, p.quality()))
                                        .color(match p.max_el {
                                            e if e >= 30.0 => theme::GREEN,
                                            e if e >= 15.0 => theme::TEXT,
                                            _ => theme::CYAN_DIM,
                                        }),
                                );
                                ui.end_row();
                            }
                        },
                    );
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("AOS/LOS are azimuths at the horizon. Times are UTC.")
                            .color(theme::LINE_LIT)
                            .size(10.0),
                    );
                }
            }

            // The operator's own entry wins over the built-in table: they have
            // either corrected it or added a satellite it never knew about.
            let (freqs, mine) = match st.sat_cfg.freqs_for(id) {
                Some(f) => (Some(f), true),
                None => (sdroxide_solar::satfreq::builtin_for(id), false),
            };
            freq_table(ui, freqs, mine, &cache.name);
        });
    if !open {
        st.selected_sat = None;
    }
}

/// The satellite's published frequencies, under the pass table.
///
/// Knowing when a bird comes over is only half of working it; the other half is
/// what to tune to, and that is a table an operator would otherwise have open
/// in a browser next to the radio.
fn freq_table(
    ui: &mut egui::Ui,
    freqs: Option<&sdroxide_solar::SatFreqs>,
    mine: bool,
    tracked_name: &str,
) {
    let Some(freqs) = freqs.filter(|f| f.usable_links().next().is_some()) else {
        ui.add_space(6.0);
        ui.label(
            RichText::new("No frequencies on file for this one — add them in Settings ▸ TLE.")
                .color(theme::LINE_LIT)
                .size(10.0),
        );
        return;
    };

    ui.add_space(8.0);
    ui.separator();
    ui.add_space(4.0);
    let heading = if mine { "FREQUENCIES  ·  YOURS" } else { "FREQUENCIES" };
    ui.label(RichText::new(heading).color(theme::CYAN_DIM).size(9.5).strong());
    // The published designator can differ from what the element set calls it;
    // showing it means a table entry keyed to the wrong catalogue number is
    // visible rather than quietly presenting the wrong satellite's frequencies.
    if !freqs.name.trim().is_empty() && !freqs.name.eq_ignore_ascii_case(tracked_name) {
        ui.label(
            RichText::new(format!("published as {}", freqs.name)).color(theme::LINE_LIT).size(10.0),
        );
    }
    ui.add_space(2.0);

    egui::Grid::new("solar-freq-grid").num_columns(4).spacing([14.0, 3.0]).show(ui, |ui| {
        for h in ["LINK", "DOWNLINK (MHz)", "UPLINK (MHz)", "MODE"] {
            ui.label(RichText::new(h).color(theme::CYAN_DIM).size(9.5).strong());
        }
        ui.end_row();
        for l in freqs.links.iter().filter(|l| !l.is_empty()) {
            let mut label = RichText::new(&l.label).color(theme::TEXT);
            if !l.note.is_empty() {
                label = label.color(theme::TEXT_STRONG);
            }
            let resp = ui.label(label);
            if !l.note.is_empty() {
                resp.on_hover_text(&l.note);
            }
            // The downlink is what gets tuned first, so it leads.
            ui.label(
                RichText::new(l.downlink.map_or_else(|| "—".into(), |b| b.to_string()))
                    .color(theme::GREEN),
            );
            ui.label(
                RichText::new(l.uplink.map_or_else(|| "—".into(), |b| b.to_string()))
                    .color(theme::YELLOW),
            );
            ui.label(RichText::new(&l.mode).color(theme::TEXT));
            ui.end_row();
        }
    });

    // Everything an operator has to know before keying up sits in the notes, so
    // they go on screen rather than only in a tooltip.
    for l in freqs.links.iter().filter(|l| !l.note.is_empty() && !l.is_empty()) {
        ui.label(
            RichText::new(format!("{} — {}", l.label, l.note)).color(theme::LINE_LIT).size(10.0),
        );
    }
    ui.add_space(2.0);
    ui.label(
        RichText::new("Doppler shifts these by a few kHz across a LEO pass.")
            .color(theme::LINE_LIT)
            .size(10.0),
    );
}

/// `HH:MM` — the date is already on the start column.
fn hhmm(unix: i64) -> String {
    let (_, _, _, h, m, _) = sdroxide_types::utc_ymd_hms(unix);
    format!("{h:02}:{m:02}")
}

/// A big dot-matrix UTC clock in the top-left corner.
///
/// UTC because everything else in the window is: the ephemeris, the DONKI
/// timestamps, the arrival estimates and the FT8 slot boundaries. A local-time
/// clock here would be the only thing on screen in a different frame.
fn clock(ui: &egui::Ui, rect: egui::Rect, sim_now: f64, scrubbed: bool) -> Option<egui::Rect> {
    use super::dotmatrix;

    let (_, _, _, h, m, s) = sdroxide_types::utc_ymd_hms(sim_now as i64);
    let text = format!("{h:02}:{m:02}:{s:02}");

    // Scale with the window, but never so large it competes with the scene.
    let pitch = (rect.width() * 0.0085).clamp(2.6, 7.0);
    let size = dotmatrix::size(&text, pitch);
    let label_pitch = pitch * 0.42;
    let label = if scrubbed { "-- SIM" } else { "UTC" };
    let label_size = dotmatrix::size(label, label_pitch);

    let pad = egui::vec2(12.0, 9.0);
    let panel = egui::Rect::from_min_size(
        rect.left_top() + egui::vec2(12.0, 12.0),
        egui::vec2(size.x, size.y + label_size.y + 6.0) + pad * 2.0,
    );
    if !rect.contains_rect(panel) {
        return None;
    }

    ui.painter().rect_filled(panel, 0, theme::BG_DEEP.gamma_multiply(0.72));
    chrome::paint_cut_border(
        ui.painter(),
        panel,
        if scrubbed { theme::YELLOW } else { theme::LINE_LIT },
        egui::Color32::TRANSPARENT,
    );

    // Unlit dots at a low alpha are what make this read as a physical display
    // rather than as text in a blocky face.
    let on = if scrubbed { theme::YELLOW } else { theme::CYAN };
    let off = on.gamma_multiply(0.11);
    dotmatrix::draw(ui.painter(), panel.min + pad, &text, pitch, on, off);
    dotmatrix::draw(
        ui.painter(),
        panel.min + pad + egui::vec2(0.0, size.y + 6.0),
        label,
        label_pitch,
        on.gamma_multiply(0.6),
        egui::Color32::TRANSPARENT,
    );
    Some(panel)
}

/// The satellite search box, under the clock.
///
/// Ninety satellites is far too many to find one by reading labels, and the
/// ones that are not in the curated set have no label at all until `ALL SATS`
/// is on — at which point there are ninety unlabelled dots. Typing a designator
/// pulls that satellite out of the crowd with its orbit and its name, whether
/// or not it was being drawn a moment ago.
///
/// Hidden when the satellite layer is off, because a search that highlights
/// things nothing is drawing would look broken.
fn sat_search(
    ui: &egui::Ui,
    st: &mut SolarUi,
    data: Option<&SolarData>,
    rect: egui::Rect,
    clock_rect: Option<egui::Rect>,
) {
    let Some(clock_rect) = clock_rect else { return };
    if !st.layer(layer::SATS) {
        // Leaving text behind in a hidden box would keep highlighting after the
        // layer came back, with nothing on screen to say why.
        st.sat_search.clear();
        return;
    }

    let width = clock_rect.width().max(210.0);
    let area = egui::Rect::from_min_size(
        clock_rect.left_bottom() + egui::vec2(0.0, 6.0),
        egui::vec2(width, 30.0),
    );
    if !rect.contains_rect(area) {
        return;
    }

    // Matches are counted from the same predicate the scene highlights with, so
    // "3 of 94" can never disagree with what is lit up.
    let (hits, total) = match data {
        Some(d) => d.satellites().fold((0usize, 0usize), |(h, n), sat| {
            (h + st.sat_search_hit(&sat.name, sat.norad_id) as usize, n + 1)
        }),
        None => (0, 0),
    };
    let query = st.sat_search.trim().to_string();
    let mut clear = false;
    let mut open: Option<u64> = None;

    egui::Area::new(egui::Id::new("solar-sat-search"))
        .order(egui::Order::Foreground)
        .fixed_pos(area.min)
        .show(ui.ctx(), |ui| {
            egui::Frame::new()
                .fill(theme::BG_DEEP.gamma_multiply(0.72))
                .inner_margin(egui::Margin::symmetric(8, 5))
                .show(ui, |ui| {
                    ui.set_width(width - 16.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 5.0;
                        ui.label(RichText::new("⌕").color(theme::CYAN_DIM).size(14.0));
                        let edit = ui.add(
                            egui::TextEdit::singleline(&mut st.sat_search)
                                .desired_width(width - 62.0)
                                .hint_text("satellite")
                                .text_color(theme::TEXT_STRONG),
                        );
                        // Enter on a single match opens its pass table, which is
                        // what you were looking the satellite up for.
                        if edit.lost_focus()
                            && ui.input(|i| i.key_pressed(egui::Key::Enter))
                            && hits == 1
                        {
                            open = data.and_then(|d| {
                                d.satellites()
                                    .find(|s| st.sat_search_hit(&s.name, s.norad_id))
                                    .map(|s| s.norad_id)
                            });
                        }
                        if !query.is_empty()
                            && ui.button("✕").on_hover_text("Clear the search").clicked()
                        {
                            clear = true;
                        }
                    });
                    if !query.is_empty() {
                        let (text, colour) = match hits {
                            0 => ("no match".to_string(), theme::PINK),
                            n => (format!("{n} of {total} tracked"), theme::YELLOW),
                        };
                        ui.label(RichText::new(text).color(colour).size(10.0));
                    }
                });
        });

    if clear {
        st.sat_search.clear();
    }
    if let Some(id) = open {
        st.selected_sat = Some(id);
    }
}

/// The propagation numbers, top right: MUF at the QTH, K and A, the 10.7 cm
/// flux and the current GOES X-ray level.
///
/// Returns the y coordinate the next panel down the right-hand edge should
/// start at, so the aurora panel stacks under it whether or not this one drew.
fn weather_panel(
    ui: &egui::Ui,
    st: &SolarUi,
    data: Option<&SolarData>,
    rect: egui::Rect,
    now: i64,
) -> f32 {
    let top = rect.top() + 12.0;
    let Some(d) = data else { return top };
    let w = &d.weather;

    // (label, value, colour). Colours say what the number means for the bands,
    // which is the only reason an operator is reading them.
    let mut rows: Vec<(String, String, egui::Color32)> = Vec::new();

    if let Some((lat, lon)) = st.qth {
        match sdroxide_solar::indices::estimate_muf(&w.ionosondes, lat, lon, now) {
            Some(m) => rows.push((
                "MUF".into(),
                format!("{:.1} MHz", m.muf_mhz),
                match m.muf_mhz {
                    f if f >= 24.0 => theme::GREEN,
                    f if f >= 14.0 => theme::CYAN,
                    _ => theme::YELLOW,
                },
            )),
            None => rows.push(("MUF".into(), "no sounder".into(), theme::LINE_LIT)),
        }
    }
    if let Some(g) = &w.geomagnetic {
        let color = match g.kp {
            k if k >= 5.0 => theme::PINK,
            k if k >= 4.0 => theme::YELLOW,
            _ => theme::GREEN,
        };
        rows.push(("Kp / A".into(), format!("{:.1} / {:.0}", g.kp, g.a_index), color));
    }
    if let Some(f) = &w.flux {
        rows.push((
            "F10.7".into(),
            format!("{:.0} sfu", f.sfu),
            match f.sfu {
                v if v >= 150.0 => theme::GREEN,
                v if v >= 90.0 => theme::CYAN,
                _ => theme::YELLOW,
            },
        ));
    }
    if let Some(x) = &w.xray {
        rows.push((
            "X-ray".into(),
            x.class.clone(),
            if x.causes_hf_absorption() { theme::PINK } else { theme::CYAN_DIM },
        ));
    }
    if rows.is_empty() {
        return top;
    }

    let font = egui::FontId::proportional(12.0);
    let small = egui::FontId::proportional(10.0);
    let p = ui.painter();
    let laid: Vec<_> = rows
        .iter()
        .map(|(k, v, c)| {
            (
                p.layout_no_wrap(k.clone(), small.clone(), theme::CYAN_DIM),
                p.layout_no_wrap(v.clone(), font.clone(), *c),
            )
        })
        .collect();
    let key_w = laid.iter().map(|(k, _)| k.size().x).fold(0.0f32, f32::max);
    let val_w = laid.iter().map(|(_, v)| v.size().x).fold(0.0f32, f32::max);
    let row_h = laid.iter().map(|(_, v)| v.size().y + 3.0).fold(0.0f32, f32::max);

    // A one-line caveat under the MUF: it is interpolated from ionosondes that
    // may be a long way off, and saying so costs one line.
    let note = st
        .qth
        .and_then(|(lat, lon)| sdroxide_solar::indices::estimate_muf(&w.ionosondes, lat, lon, now))
        .map(|m| {
            p.layout_no_wrap(
                format!("{} · {:.0} km", m.confidence(), m.nearest_km),
                small.clone(),
                theme::LINE_LIT,
            )
        });

    let pad = 10.0;
    let width = key_w + val_w + 18.0 + pad * 2.0;
    let width = note.as_ref().map_or(width, |n| width.max(n.size().x + pad * 2.0));
    let height =
        rows.len() as f32 * row_h + note.as_ref().map_or(0.0, |n| n.size().y + 4.0) + pad * 2.0;
    let panel = egui::Rect::from_min_size(
        egui::pos2(rect.right() - width - 12.0, top),
        egui::vec2(width, height),
    );
    if !rect.contains_rect(panel) {
        return top;
    }

    p.rect_filled(panel, 0, theme::FILL.gamma_multiply(0.82));
    chrome::paint_cut_border(p, panel, theme::LINE_LIT, egui::Color32::TRANSPARENT);
    let mut y = panel.top() + pad;
    for ((key, val), (_, _, color)) in laid.iter().zip(&rows) {
        p.galley(egui::pos2(panel.left() + pad, y + 2.0), key.clone(), theme::CYAN_DIM);
        p.galley(egui::pos2(panel.right() - pad - val.size().x, y), val.clone(), *color);
        y += row_h;
    }
    if let Some(n) = note {
        p.galley(egui::pos2(panel.left() + pad, y + 2.0), n, theme::LINE_LIT);
    }
    panel.bottom() + 8.0
}

/// Aurora, under the propagation numbers: how much power is going into each
/// oval, how far towards the equator it reaches, whether it is over your head,
/// and what the planetary K forecast says about tonight.
///
/// The colours here mean the same thing they do everywhere else in the window —
/// green quiet, yellow worth watching, pink a storm.
fn aurora_panel(
    ui: &egui::Ui,
    st: &SolarUi,
    data: Option<&SolarData>,
    rect: egui::Rect,
    top: f32,
    now: i64,
) {
    use sdroxide_solar::{HemisphericPower, aurora};

    let Some(d) = data else { return };
    // Nothing to say until one of the three aurora feeds has landed. Drawing an
    // empty box would imply the aurora had been measured and found absent.
    if d.aurora.is_none() && d.aurora_power.is_none() && d.kp_forecast.is_empty() {
        return;
    }
    let kp_color = |kp: f64| match kp {
        k if k >= 5.0 => theme::PINK,
        k if k >= 4.0 => theme::YELLOW,
        _ => theme::GREEN,
    };

    // Both hemispheres are reported throughout: without a QTH neither is more
    // relevant than the other, and with one the QTH row already says what
    // matters locally.
    let mut rows: Vec<(String, String, egui::Color32)> = Vec::new();
    if let Some(power) = &d.aurora_power {
        let worst = power.north_gw.max(power.south_gw);
        let color = match HemisphericPower::index(worst) {
            8..=10 => theme::PINK,
            6..=7 => theme::YELLOW,
            _ => theme::GREEN,
        };
        rows.push((
            "power N/S".into(),
            format!("{:.0} / {:.0} GW", power.north_gw, power.south_gw),
            color,
        ));
        rows.push((
            "activity".into(),
            format!("{} · HPI {}", HemisphericPower::words(worst), HemisphericPower::index(worst)),
            color,
        ));
    }
    if let Some(oval) = &d.aurora {
        // The edge is where it stops being worth looking, so it is the number
        // an operator compares their own latitude against.
        let edge = |n: bool| {
            oval.equatorward_edge(n, aurora::EDGE_PCT)
                .map(|lat| format!("{:.0}°{}", lat.abs(), if n { "N" } else { "S" }))
        };
        // An oval too weak to reach the contour anywhere in one hemisphere is a
        // real state, and one worth reading as a dash rather than as a row that
        // silently disappeared.
        let (n, s) = (edge(true), edge(false));
        if n.is_some() || s.is_some() {
            let show = |e: Option<String>| e.unwrap_or_else(|| "—".into());
            rows.push(("edge N/S".into(), format!("{} / {}", show(n), show(s)), theme::CYAN));
        }
        if let Some((lat, lon)) = st.qth {
            let pct = oval.probability(lat, lon);
            let color = match pct {
                p if p >= 25.0 => theme::PINK,
                p if p >= aurora::EDGE_PCT => theme::YELLOW,
                p if p >= aurora::NOISE_FLOOR_PCT => theme::GREEN,
                _ => theme::LINE_LIT,
            };
            rows.push((st.qth_grid.clone(), format!("{pct:.0} %"), color));
        }
    }
    // The forecast half: the worst three-hour bin still ahead of us, and how
    // far away it is.
    let peak = aurora::peak_forecast(&d.kp_forecast, now, 24 * 3600);
    if let Some(p) = peak {
        let in_h = (p.unix - now).max(0) as f64 / 3600.0;
        rows.push((
            "Kp peak 24 h".into(),
            if in_h < 1.5 {
                format!("{:.1} now", p.kp)
            } else {
                format!("{:.1} in {in_h:.0} h", p.kp)
            },
            kp_color(p.kp),
        ));
        rows.push((
            "viewline".into(),
            format!("{:.0}° geomag", aurora::viewline_geomagnetic_lat(p.kp)),
            kp_color(p.kp),
        ));
    }
    if rows.is_empty() {
        return;
    }

    // The forecast strip: one bar per three-hour bin over the next day. Eight
    // numbers in a column is a table nobody reads; the same eight as a shape is
    // "it picks up after midnight" at a glance.
    let bins: Vec<_> = aurora::upcoming(&d.kp_forecast, now, 24 * 3600).take(8).collect();
    const BAR_W: f32 = 13.0;
    const BAR_GAP: f32 = 3.0;
    const STRIP_H: f32 = 26.0;

    let font = egui::FontId::proportional(12.0);
    let small = egui::FontId::proportional(10.0);
    let p = ui.painter();
    let laid: Vec<_> = rows
        .iter()
        .map(|(k, v, c)| {
            (
                p.layout_no_wrap(k.clone(), small.clone(), theme::CYAN_DIM),
                p.layout_no_wrap(v.clone(), font.clone(), *c),
            )
        })
        .collect();
    let key_w = laid.iter().map(|(k, _)| k.size().x).fold(0.0f32, f32::max);
    let val_w = laid.iter().map(|(_, v)| v.size().x).fold(0.0f32, f32::max);
    let row_h = laid.iter().map(|(_, v)| v.size().y + 3.0).fold(0.0f32, f32::max);

    // What the picture is valid for, never what time it is now: OVATION is a
    // forecast for about forty minutes ahead and the grid may be an hour old.
    let footer = d.aurora.as_ref().map(|o| {
        let age = (now - o.observed_unix).max(0);
        p.layout_no_wrap(
            format!("valid {} · {} old", timefmt::ymd_hm(o.forecast_unix), timefmt::age(age)),
            small.clone(),
            theme::LINE_LIT,
        )
    });

    let title = p.layout_no_wrap("AURORA".into(), small.clone(), theme::CYAN_DIM);
    let pad = 10.0;
    let strip_w =
        if bins.is_empty() { 0.0 } else { bins.len() as f32 * (BAR_W + BAR_GAP) - BAR_GAP };
    let width =
        (key_w + val_w + 18.0).max(strip_w).max(footer.as_ref().map_or(0.0, |f| f.size().x))
            + pad * 2.0;
    let height = title.size().y
        + 5.0
        + rows.len() as f32 * row_h
        // Strip: the gap above it, the bars, and the hour stamps under them.
        + if bins.is_empty() { 0.0 } else { STRIP_H + 18.0 }
        + footer.as_ref().map_or(0.0, |f| f.size().y + 4.0)
        + pad * 2.0;

    let panel = egui::Rect::from_min_size(
        egui::pos2(rect.right() - width - 12.0, top),
        egui::vec2(width, height),
    );
    // Same rule as every other readout in this window: if it does not fit, it
    // is not drawn. A panel clipped by the viewport edge is worse than none.
    if !rect.contains_rect(panel) {
        return;
    }

    p.rect_filled(panel, 0, theme::FILL.gamma_multiply(0.82));
    chrome::paint_cut_border(p, panel, theme::LINE_LIT, egui::Color32::TRANSPARENT);

    let mut y = panel.top() + pad;
    let title_h = title.size().y;
    p.galley(egui::pos2(panel.left() + pad, y), title, theme::CYAN_DIM);
    y += title_h + 5.0;
    for ((key, val), (_, _, color)) in laid.iter().zip(&rows) {
        p.galley(egui::pos2(panel.left() + pad, y + 2.0), key.clone(), theme::CYAN_DIM);
        p.galley(egui::pos2(panel.right() - pad - val.size().x, y), val.clone(), *color);
        y += row_h;
    }

    if !bins.is_empty() {
        y += 6.0;
        let base = y + STRIP_H;
        for (i, bin) in bins.iter().enumerate() {
            let x = panel.left() + pad + i as f32 * (BAR_W + BAR_GAP);
            let h = (bin.kp / 9.0) as f32 * STRIP_H;
            // An unlit socket under every bar, so a quiet forecast still reads
            // as a scale rather than as missing data.
            p.rect_filled(
                egui::Rect::from_min_max(egui::pos2(x, y), egui::pos2(x + BAR_W, base)),
                0,
                theme::LINE.gamma_multiply(0.55),
            );
            p.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(x, base - h.max(1.0)),
                    egui::pos2(x + BAR_W, base),
                ),
                0,
                kp_color(bin.kp),
            );
        }
        // Hours under the ends, so the strip has a time axis without eight
        // labels fighting for room.
        let stamp = |unix: i64| {
            let (_, _, _, h, _, _) = sdroxide_types::utc_ymd_hms(unix);
            format!("{h:02}z")
        };
        p.text(
            egui::pos2(panel.left() + pad, base + 2.0),
            egui::Align2::LEFT_TOP,
            stamp(bins[0].unix),
            small.clone(),
            theme::LINE_LIT,
        );
        p.text(
            egui::pos2(panel.left() + pad + strip_w, base + 2.0),
            egui::Align2::RIGHT_TOP,
            stamp(bins[bins.len() - 1].unix),
            small.clone(),
            theme::LINE_LIT,
        );
        y = base + 12.0;
    }

    if let Some(f) = footer {
        p.galley(egui::pos2(panel.left() + pad, y + 2.0), f, theme::LINE_LIT);
    }
}

/// Bottom-left readout: where the Sun is, where it is over the operator, and
/// what the feed knows.
fn info_card(
    ui: &egui::Ui,
    st: &SolarUi,
    data: Option<&SolarData>,
    rect: egui::Rect,
    sim_now: f64,
) {
    use sdroxide_solar::ephem;
    let jd = ephem::julian_day(sim_now);
    let (_, b0, l0) = ephem::solar_p_b0_l0(jd);
    let (slat, slon) = ephem::subsolar_point(jd);

    let mut lines = vec![
        format!("{}  UTC", timefmt::ymd_hm(sim_now as i64)),
        format!("sub-solar  {slat:+.1}°  {slon:+.1}°"),
        format!("B0 {b0:+.2}°   L0 {l0:.1}°"),
    ];
    if let Some((lat, lon)) = st.qth {
        let (el, az) = sun_elevation_azimuth(lat, lon, slat, slon);
        let state = if el > 0.0 { "day" } else { "night" };
        lines.push(format!("{}  sun {el:+.0}° el {az:.0}° az ({state})", st.qth_grid));
    }
    if let Some(d) = data {
        let visible = d
            .cmes
            .iter()
            .filter(|e| {
                e.analysis.as_ref().is_some_and(|a| {
                    let age = sim_now as i64 - a.t21_5_unix;
                    (0..(st.view.cme_window_h as i64 * 3600)).contains(&age)
                })
            })
            .count();
        lines.push(format!("{visible} CME · {} spots", d.regions.len()));
    }
    if st.view.auto {
        let phase = if st.tour.in_transit() { "→ " } else { "" };
        lines.push(format!("AUTO  {phase}{}", st.tour.leg_name()));
    }

    let font = egui::FontId::proportional(11.5);
    let galleys: Vec<_> = lines
        .iter()
        .map(|l| ui.painter().layout_no_wrap(l.clone(), font.clone(), theme::TEXT))
        .collect();
    let w = galleys.iter().map(|g| g.size().x).fold(0.0f32, f32::max) + 20.0;
    let h = galleys.iter().map(|g| g.size().y + 2.0).sum::<f32>() + 16.0;
    let card = egui::Rect::from_min_size(
        egui::pos2(rect.left() + 12.0, rect.bottom() - h - 12.0),
        egui::vec2(w, h),
    );
    if !rect.contains_rect(card) {
        return; // too small a window to be worth crowding
    }
    ui.painter().rect_filled(card, 0, theme::FILL.gamma_multiply(0.82));
    chrome::paint_cut_border(ui.painter(), card, theme::LINE_LIT, egui::Color32::TRANSPARENT);
    let mut y = card.top() + 8.0;
    for g in galleys {
        let dy = g.size().y + 2.0;
        ui.painter().galley(egui::pos2(card.left() + 10.0, y), g, theme::TEXT);
        y += dy;
    }
}

/// The award layer's key, bottom right: what each colour on the globe means,
/// and how many entities are in each state.
///
/// A heat map with no legend is decoration. This one is small, and it only
/// exists while the layer it explains is switched on.
fn award_panel(ui: &egui::Ui, st: &SolarUi, rect: egui::Rect) {
    if !st.layer(layer::AWARDS) || st.awards.is_empty() {
        return;
    }
    let (missing, worked, confirmed) = sdroxide_types::coverage_counts(&st.awards);
    let rows = [
        ("missing", missing, egui::Color32::from_rgb(0xff, 0x5a, 0x28)),
        ("worked", worked, theme::YELLOW),
        ("confirmed", confirmed, theme::GREEN),
    ];

    let p = ui.painter();
    let font = egui::FontId::proportional(11.5);
    let cap = egui::FontId::proportional(9.5);
    let galleys: Vec<_> = rows
        .iter()
        .map(|(label, n, _)| p.layout_no_wrap(format!("{label}  {n}"), font.clone(), theme::TEXT))
        .collect();
    let title = p.layout_no_wrap("DXCC COVERAGE".into(), cap, theme::CYAN_DIM);

    const SWATCH: f32 = 9.0;
    let w = galleys.iter().map(|g| g.size().x).fold(title.size().x, f32::max) + SWATCH + 26.0;
    let h = galleys.iter().map(|g| g.size().y + 3.0).sum::<f32>() + title.size().y + 18.0;
    let panel = egui::Rect::from_min_size(
        egui::pos2(rect.right() - w - 12.0, rect.bottom() - h - 12.0),
        egui::vec2(w, h),
    );
    if !rect.contains_rect(panel) {
        return; // too small a window to be worth crowding
    }
    p.rect_filled(panel, 0, theme::FILL.gamma_multiply(0.82));
    chrome::paint_cut_border(p, panel, theme::LINE_LIT, egui::Color32::TRANSPARENT);

    let mut y = panel.top() + 7.0;
    let x = panel.left() + 10.0;
    let title_h = title.size().y;
    p.galley(egui::pos2(x, y), title, theme::CYAN_DIM);
    y += title_h + 4.0;
    for (g, (_, _, color)) in galleys.into_iter().zip(rows) {
        let dy = g.size().y + 3.0;
        p.circle_filled(egui::pos2(x + SWATCH * 0.5, y + g.size().y * 0.5), SWATCH * 0.42, color);
        p.galley(egui::pos2(x + SWATCH + 8.0, y), g, theme::TEXT);
        y += dy;
    }
}

/// The banner that justifies the whole window: a CME whose cone contains the
/// Earth, with an arrival estimate.
/// One line saying what the cloud deck is, and what about it is not measured.
///
/// The hour it shows is the hour the mosaic is *of*, which is over an hour
/// behind the clock — the same discipline the aurora footer keeps, and for the
/// same reason: a picture presented as current when it is not is worse than no
/// picture. And it says the lightning is simulated, which is not optional. The
/// storms are real and come out of the imagery; the individual flashes are
/// invented, and a globe that flickers with plausible-looking strikes has to
/// admit that rather than let anyone read them as strikes.
fn clouds_note(ui: &egui::Ui, st: &SolarUi, data: Option<&SolarData>, rect: egui::Rect, now: i64) {
    if !st.layer(layer::CLOUDS) {
        return;
    }
    let Some(field) = data.and_then(|d| d.clouds.as_ref()) else { return };

    let channels = if field.has_visible { "IR+VIS" } else { "IR only" };
    let storms = match field.cells.len() {
        0 => "no deep convection".to_string(),
        1 => "1 storm".to_string(),
        n => format!("{n} storms"),
    };
    let text = format!(
        "CLOUDS  {}  ·  {} old  ·  {channels}  ·  {storms}  ·  lightning simulated",
        timefmt::ymd_hm(field.frame_unix),
        timefmt::age((now - field.frame_unix).max(0)),
    );

    let p = ui.painter();
    let galley = p.layout_no_wrap(text, egui::FontId::proportional(10.5), theme::LINE_LIT);
    if galley.size().x > rect.width() - 40.0 {
        return;
    }
    p.galley(
        egui::pos2(rect.left() + 14.0, rect.bottom() - galley.size().y - 8.0),
        galley,
        theme::LINE_LIT,
    );
}

fn impact_banner(ui: &egui::Ui, data: Option<&SolarData>, rect: egui::Rect, now: i64) {
    let Some(d) = data else { return };
    // The soonest arrival that has not already happened.
    let mut best: Option<(&sdroxide_solar::CmeEvent, sdroxide_solar::Impact)> = None;
    for e in &d.cmes {
        let Some(a) = &e.analysis else { continue };
        let Some(hit) = sdroxide_solar::earth_impact(a) else { continue };
        // Keep it on screen for a few hours past the estimate, since arrival
        // estimates are routinely off by that much.
        if hit.eta_unix < now - 6 * 3600 {
            continue;
        }
        if best.as_ref().is_none_or(|(_, b)| hit.eta_unix < b.eta_unix) {
            best = Some((e, hit));
        }
    }
    let Some((event, hit)) = best else { return };
    let a = event.analysis.as_ref().expect("filtered above");

    let hours = (hit.eta_unix - now) as f64 / 3600.0;
    let when = if hours >= 0.0 {
        format!("ETA {} (+{hours:.0} h)", timefmt::ymd_hm(hit.eta_unix))
    } else {
        format!("arrival was {} ({:.0} h ago)", timefmt::ymd_hm(hit.eta_unix), -hours)
    };
    let glancing = if hit.directness(a.half_angle_deg) < 0.35 { " · glancing" } else { "" };
    let estimated = if a.estimated { " · direction estimated" } else { "" };
    let text = format!(
        "EARTH-DIRECTED CME  {}  ·  {:.0} km/s  ·  {when}{glancing}{estimated}",
        timefmt::ymd_hm(a.t21_5_unix),
        a.speed_km_s,
    );

    // Hazard-striped tabs at both ends, the same mark the manual uses on every
    // section header. A CME arrival is the one thing in this window that wants
    // to be noticed without being read first.
    const TAB_W: f32 = 22.0;
    let font = egui::FontId::proportional(13.0);
    let galley = ui.painter().layout_no_wrap(text, font, theme::TEXT_STRONG);
    let size = galley.size() + egui::vec2(28.0 + TAB_W * 2.0, 15.0);
    if size.x > rect.width() - 24.0 {
        return;
    }
    let banner = egui::Rect::from_center_size(
        egui::pos2(rect.center().x, rect.bottom() - size.y * 0.5 - 14.0),
        size,
    );
    let p = ui.painter();
    p.rect_filled(banner, 0, theme::CQ_BG.gamma_multiply(0.94));
    for tab in [
        egui::Rect::from_min_size(banner.min, egui::vec2(TAB_W, banner.height())),
        egui::Rect::from_min_size(
            egui::pos2(banner.right() - TAB_W, banner.top()),
            egui::vec2(TAB_W, banner.height()),
        ),
    ] {
        chrome::hazard_stripes(p, tab, 7.0);
    }
    chrome::paint_cut_border(p, banner, theme::PINK, egui::Color32::TRANSPARENT);
    p.galley(
        egui::pos2(banner.left() + TAB_W + 14.0, banner.top() + 7.0),
        galley,
        theme::TEXT_STRONG,
    );
}

/// Solar elevation and azimuth at a location, from the sub-solar point.
///
/// Both points are on the same sphere, so this is the great-circle geometry the
/// FT8 map already uses for bearings — elevation is 90° minus the angular
/// distance to the sub-solar point.
fn sun_elevation_azimuth(lat: f64, lon: f64, slat: f64, slon: f64) -> (f64, f64) {
    let (p1, p2) = (lat.to_radians(), slat.to_radians());
    let dl = (slon - lon).to_radians();
    let cos_c = p1.sin() * p2.sin() + p1.cos() * p2.cos() * dl.cos();
    let elevation = cos_c.clamp(-1.0, 1.0).asin().to_degrees();
    let az = (dl.sin() * p2.cos())
        .atan2(p1.cos() * p2.sin() - p1.sin() * p2.cos() * dl.cos())
        .to_degrees();
    (elevation, (az + 360.0) % 360.0)
}

/// Fly the AUTO tour, using real elapsed time so the pacing is frame-rate
/// independent.
fn advance_tour(ui: &egui::Ui, st: &mut SolarUi, sim_now: f64, dt: f32) {
    if !st.view.auto {
        // Hand the pivot back to the focus chips.
        st.focus_override = None;
        return;
    }
    let b = super::scene::bodies(st, sim_now);
    // The contact being worked pre-empts the tour — but only while its arc is
    // actually on screen: with the layer switched off there is nothing there
    // for the camera to fly to.
    let qso = st
        .layer(crate::view::solar_layer::QSO)
        .then(|| st.qth.zip(st.digi.dx))
        .flatten()
        .map(|(home, dx)| super::camera::QsoPath { home, dx });
    // `Tour` is `Copy`, so step a local and write it back rather than fighting
    // the borrow of `st.view` inside it.
    let mut tour = st.tour;
    let pivot = tour.step(&mut st.view, &b, dt, qso);
    st.tour = tour;
    st.focus_override = Some(pivot);
    ui.ctx().request_repaint();
}

/// Drag to rotate, scroll to zoom, double-click to reframe. Any of them cancels
/// the animated tour — the user taking the controls is the signal to stop.
fn interact(ui: &egui::Ui, st: &mut SolarUi, resp: &egui::Response) {
    let mut touched = false;

    if resp.dragged_by(egui::PointerButton::Primary) {
        let d = resp.drag_delta();
        st.view.yaw -= d.x * 0.006;
        st.view.pitch = (st.view.pitch + d.y * 0.006)
            .clamp(-super::camera::PITCH_LIMIT, super::camera::PITCH_LIMIT);
        touched |= d != egui::Vec2::ZERO;
    }

    if resp.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll != 0.0 {
            // Multiplicative, so a wheel click covers the same visual fraction
            // whether you are 3 Gm or 3 AU out.
            st.view.dist *= (1.0 - scroll * 0.0022).clamp(0.4, 2.5);
            touched = true;
        }
    }

    // Clamping needs the focus radius, which only the scene knows; `Camera`
    // re-clamps anyway, so here just keep the stored value sane.
    st.view.dist = st.view.dist.clamp(1e-5, super::camera::MAX_DIST);

    if touched {
        st.view.auto = false;
    }
    // Continuous repaint only while something is actually moving; otherwise the
    // window idles and is woken by input or by the data feed.
    if touched || st.view.auto || resp.is_pointer_button_down_on() {
        ui.ctx().request_repaint();
    }
}
