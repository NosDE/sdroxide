//! The TX Audio window: the transmit voice chain's controls.
//!
//! ```text
//! mic → [denoise] → Gate → EQ → De-esser → Compressor → Output
//! ```
//!
//! The chain itself runs in the engine (see `sdroxide_dsp::VoiceChain`); this
//! window edits a copy of [`TxVoiceState`] and sends the whole of it back on
//! every change, the same "always whole, never a delta" way the EQ in Settings
//! used to. Everything it draws — the EQ response, the transfer curves — comes
//! from the same functions the DSP runs, in `sdroxide-types`, so the curve on
//! screen is the one the audio meets.
//!
//! The test phrase (REC / PLAY) is recorded raw by the engine and played back
//! through the chain as it is set *now*, so the operator can say the same
//! sentence once and then tune against it without keying the transmitter.

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Pos2, Rect, RichText, Sense, Slider, Stroke,
    StrokeKind, Vec2, pos2, vec2,
};
use sdroxide_types::{
    Command, TX_VOICE_TEST_MAX_S, TxCompState, TxDeEssState, TxEqBand, TxEqEdge, TxEqState,
    TxGateState, TxOutputState, TxVoiceMeters, TxVoiceProfile, TxVoiceStage, TxVoiceState,
    TxVoiceTest,
};

use crate::app::SdroxideApp;

/// The rate the chain runs at — what the EQ curve is evaluated against.
const CHAIN_RATE: f64 = 48_000.0;

/// Which page the right-hand side shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Page {
    #[default]
    Gate,
    Eq,
    DeEss,
    Comp,
    Output,
    Profiles,
}

impl Page {
    fn of(stage: TxVoiceStage) -> Page {
        match stage {
            TxVoiceStage::Gate => Page::Gate,
            TxVoiceStage::Eq => Page::Eq,
            TxVoiceStage::DeEss => Page::DeEss,
            TxVoiceStage::Comp => Page::Comp,
        }
    }
}

/// The meters as drawn: peaks that fall back slowly rather than flickering at
/// the meter rate, so a level can actually be read.
#[derive(Debug, Clone, Copy)]
struct Ballistics {
    m: TxVoiceMeters,
    last: f64,
}

impl Default for Ballistics {
    fn default() -> Self {
        Ballistics { m: TxVoiceMeters::default(), last: 0.0 }
    }
}

/// How fast a held peak falls, dB per second.
const METER_FALL_DB_S: f32 = 20.0;

impl Ballistics {
    fn update(&mut self, now: f64, fresh: Option<TxVoiceMeters>) {
        let dt = ((now - self.last).clamp(0.0, 0.5)) as f32;
        self.last = now;
        let fall = METER_FALL_DB_S * dt;
        let level = |held: f32, new: Option<f32>| {
            let decayed = if held.is_finite() { held - fall } else { held };
            match new {
                Some(n) => n.max(decayed),
                None => decayed,
            }
            .max(-120.0)
        };
        let gr = |held: f32, new: Option<f32>| {
            let decayed = (held + fall).min(0.0);
            match new {
                Some(n) => n.min(decayed),
                None => decayed,
            }
        };
        self.m.input_db = level(self.m.input_db, fresh.map(|f| f.input_db));
        self.m.output_db = level(self.m.output_db, fresh.map(|f| f.output_db));
        self.m.gate_gr_db = gr(self.m.gate_gr_db, fresh.map(|f| f.gate_gr_db));
        self.m.deess_gr_db = gr(self.m.deess_gr_db, fresh.map(|f| f.deess_gr_db));
        self.m.comp_gr_db = gr(self.m.comp_gr_db, fresh.map(|f| f.comp_gr_db));
        self.m.limiter_gr_db = gr(self.m.limiter_gr_db, fresh.map(|f| f.limiter_gr_db));
    }

    /// Whether anything is still moving, so the window keeps repainting until
    /// the meters have settled.
    fn moving(&self) -> bool {
        self.m.input_db > -100.0
            || self.m.output_db > -100.0
            || self.m.gate_gr_db < 0.0
            || self.m.deess_gr_db < 0.0
            || self.m.comp_gr_db < 0.0
            || self.m.limiter_gr_db < 0.0
    }
}

/// The window's own state: which page is up, the profile library, and the
/// meters' ballistics. Everything about the chain itself lives in the engine's
/// [`TxVoiceState`].
pub(in crate::app) struct TxAudioUi {
    pub open: bool,
    page: Page,
    profiles: Vec<TxVoiceProfile>,
    profile_name: String,
    selected_profile: Option<usize>,
    import_inbox: crate::download::LoadInbox,
    note: Option<String>,
    meters: Ballistics,
}

impl TxAudioUi {
    pub fn new(storage: Option<&dyn eframe::Storage>) -> Self {
        TxAudioUi {
            open: false,
            page: Page::default(),
            profiles: super::persist::load_tx_voice_profiles(storage),
            profile_name: String::new(),
            selected_profile: None,
            import_inbox: Default::default(),
            note: None,
            meters: Ballistics::default(),
        }
    }

    /// The library, for the browser build's periodic save.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub fn profiles(&self) -> &Vec<TxVoiceProfile> {
        &self.profiles
    }

    fn save_profiles(&self) {
        super::persist::persist_tx_voice_profiles(&self.profiles);
    }
}

impl SdroxideApp {
    pub(in crate::app) fn tx_audio_window(&mut self, ctx: &egui::Context, cmds: &mut Vec<Command>) {
        if !self.tx_audio.open {
            return;
        }
        let mut open = true;
        let mut v = self.state.tx.voice;
        let test = self.state.tx.voice_test;
        let test_len = self.state.tx.voice_test_len_s;
        let transmitting = self.state.tx.ptt || self.state.tx.tune;
        let voice_mode = self.state.rx[0].mode.allows_voice_keyer();
        let now = ctx.input(|i| i.time);
        let fresh = self.meters.as_ref().and_then(|m| m.voice);
        let ui_state = &mut self.tx_audio;
        ui_state.meters.update(now, fresh);
        pick_up_import(ui_state);

        let resp = egui::Window::new("TX Audio")
            .id(crate::layout::salted_id(ctx, "TX Audio"))
            .open(&mut open)
            .frame(crate::chrome::window_frame())
            .resizable(true)
            .default_width(crate::layout::window_w(ctx, 780.0))
            .min_width(crate::layout::window_w(ctx, 560.0))
            .show(ctx, |ui| {
                crate::chrome::window_body_bg(ui);
                header(ui, &mut v, test, test_len, transmitting, cmds);
                if !voice_mode {
                    ui.label(
                        RichText::new(
                            "This mode is not a voice mode, so nothing it sends passes \
                             through the chain. Settings made here take effect in SSB, AM \
                             and FM.",
                        )
                        .color(crate::theme::YELLOW())
                        .size(11.5),
                    );
                }
                ui.separator();
                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(190.0);
                        stage_list(ui, &mut v, &mut ui_state.page);
                    });
                    ui.separator();
                    ui.vertical(|ui| {
                        let m = ui_state.meters.m;
                        match ui_state.page {
                            Page::Gate => gate_page(ui, &mut v.gate, m),
                            Page::Eq => eq_page(ui, &mut v.eq),
                            Page::DeEss => deess_page(ui, &mut v.deess, m),
                            Page::Comp => comp_page(ui, &mut v.comp, m),
                            Page::Output => output_page(ui, &mut v.output, m),
                            Page::Profiles => profiles_page(ui, ui_state, &mut v),
                        }
                    });
                });
                ui.separator();
                meter_strip(ui, &v, ui_state.meters.m);
            });
        if let Some(r) = &resp {
            crate::chrome::paint_window_border(ctx, &r.response);
        }
        if v != self.state.tx.voice {
            let v = v.clamped();
            self.state.tx.voice = v;
            cmds.push(Command::SetTxVoice(v));
        }
        if self.tx_audio.meters.moving() || test != TxVoiceTest::Idle {
            crate::repaint::after_ms(ctx, 50);
        }
        if !open {
            // Closing the window while it is recording or playing would leave
            // the engine doing it out of sight.
            match test {
                TxVoiceTest::Recording => cmds.push(Command::TxVoiceTestRecord(false)),
                TxVoiceTest::Playing => cmds.push(Command::TxVoiceTestPlay(false)),
                TxVoiceTest::Idle => {}
            }
        }
        self.tx_audio.open = open;
    }
}

/// BYPASS, the denoiser, digital voice, and the test phrase.
fn header(
    ui: &mut egui::Ui,
    v: &mut TxVoiceState,
    test: TxVoiceTest,
    test_len: f32,
    transmitting: bool,
    cmds: &mut Vec<Command>,
) {
    ui.horizontal_wrapped(|ui| {
        if crate::chrome::chip_accent(ui, v.bypass, "BYPASS", crate::theme::ALERT(), Color32::WHITE)
            .on_hover_text(
                "Switch every stage out at once, denoiser included, so the microphone goes out \
             unprocessed. Click again to restore exactly the stages that were on — the \
             quickest A/B of what the chain is doing.",
            )
            .clicked()
        {
            v.bypass = !v.bypass;
        }
        if crate::chrome::chip(ui, v.denoise, "DENOISE")
            .on_hover_text(
                "RNNoise on the microphone, ahead of every other stage, so room noise is \
                 removed before the gate or the compressor can bring it up. Adds about \
                 10 ms of delay.",
            )
            .clicked()
        {
            v.denoise = !v.denoise;
        }
        if crate::chrome::chip(ui, v.digital_voice, "DIGITAL VOICE")
            .on_hover_text(
                "Also run FreeDV / RADE digital voice through the chain. Off by default: a \
                 vocoder is trained on natural speech and may decode worse from a heavily \
                 processed voice, so try it on the air before relying on it. TCI audio and \
                 the digital modes never pass through the chain.",
            )
            .clicked()
        {
            v.digital_voice = !v.digital_voice;
        }

        ui.add_space(12.0);
        ui.label(RichText::new("Test").weak());
        let recording = test == TxVoiceTest::Recording;
        let playing = test == TxVoiceTest::Playing;
        let rec = crate::chrome::chip_accent(
            ui,
            recording,
            " ⏺ REC ",
            crate::theme::ALERT(),
            Color32::WHITE,
        );
        let rec = rec.on_hover_text(format!(
            "Record a test phrase from the microphone — up to {TX_VOICE_TEST_MAX_S:.0} s, \
             without keying the transmitter. Click again to stop. It is kept unprocessed, \
             so PLAY can put the same phrase through every change you make."
        ));
        if rec.clicked() && !transmitting {
            cmds.push(Command::TxVoiceTestRecord(!recording));
        }
        let can_play = test_len > 0.0 && !recording && !transmitting;
        let play = crate::chrome::chip_accent_enabled(
            ui,
            can_play || playing,
            playing,
            " ▶ PLAY ",
            None,
            crate::theme::GREEN(),
            crate::theme::INK_ON_BRIGHT(),
        );
        let play = play.on_hover_text(
            "Play the test phrase through the chain as it is set now, to your speakers. \
             Adjust while it plays and hear the change. Receive audio is replaced while it \
             plays.",
        );
        if play.clicked() {
            cmds.push(Command::TxVoiceTestPlay(!playing));
        }
        let what = match test {
            TxVoiceTest::Recording => "recording…".to_string(),
            _ if test_len > 0.0 => format!("{test_len:.1} s phrase"),
            _ => "no phrase yet".to_string(),
        };
        ui.label(RichText::new(what).weak().size(11.5));
    });
    ui.label(
        RichText::new(
            "Voice modes only. TCI clients and the digital modes bypass the chain; when it \
             is in use, keep the radio's own TX EQ and processor flat.",
        )
        .weak()
        .size(11.0),
    );
}

/// The left-hand column: the reorderable stages, then the fixed ones.
fn stage_list(ui: &mut egui::Ui, v: &mut TxVoiceState, page: &mut Page) {
    ui.label(RichText::new("Chain").strong().color(crate::theme::CYAN()));
    ui.label(RichText::new("In order the audio meets them").weak().size(10.5));
    ui.add_space(4.0);
    let mut swap: Option<(usize, usize)> = None;
    for i in 0..v.order.len() {
        let stage = v.order[i];
        ui.horizontal(|ui| {
            let mut on = v.stage_enabled(stage);
            if crate::chrome::checkbox(ui, &mut on, "")
                .on_hover_text("Switch this stage on")
                .changed()
            {
                v.set_stage_enabled(stage, on);
            }
            let sel = *page == Page::of(stage);
            if ui
                .add_sized(vec2(92.0, 20.0), egui::Button::selectable(sel, stage.label()))
                .clicked()
            {
                *page = Page::of(stage);
            }
            if ui
                .add_enabled(i > 0, egui::Button::new("▲").small())
                .on_hover_text("Earlier in the chain")
                .clicked()
            {
                swap = Some((i, i - 1));
            }
            if ui
                .add_enabled(i + 1 < v.order.len(), egui::Button::new("▼").small())
                .on_hover_text("Later in the chain")
                .clicked()
            {
                swap = Some((i, i + 1));
            }
        });
    }
    if let Some((a, b)) = swap {
        v.order.swap(a, b);
    }
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let mut on = v.output.limiter;
        if crate::chrome::checkbox(ui, &mut on, "").on_hover_text("The output limiter").changed() {
            v.output.limiter = on;
        }
        if ui
            .add_sized(vec2(92.0, 20.0), egui::Button::selectable(*page == Page::Output, "Output"))
            .on_hover_text("Trim and limiter — always last")
            .clicked()
        {
            *page = Page::Output;
        }
    });
    ui.add_space(10.0);
    if ui
        .add_sized(
            vec2(130.0, 20.0),
            egui::Button::selectable(*page == Page::Profiles, "Profiles…"),
        )
        .clicked()
    {
        *page = Page::Profiles;
    }
}

// ── Shared widgets ─────────────────────────────────────────────────────────

/// One labelled control row in a page's grid.
fn knob(
    ui: &mut egui::Ui,
    label: &str,
    hover: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    suffix: &str,
    log: bool,
) {
    ui.label(label).on_hover_text(hover);
    crate::chrome::slider_readout(
        ui,
        78.0,
        Slider::new(value, range).suffix(suffix).logarithmic(log).max_decimals(1),
    )
    .on_hover_text(hover);
    ui.end_row();
}

fn page_title(ui: &mut egui::Ui, title: &str, blurb: &str) {
    ui.label(RichText::new(title).size(14.0).strong().color(crate::theme::CYAN()));
    ui.label(RichText::new(blurb).weak().size(11.0));
    ui.add_space(4.0);
}

/// A plot area: background, a frame, and a mapping from values to pixels.
struct Plot {
    rect: Rect,
    x: (f32, f32),
    y: (f32, f32),
    log_x: bool,
}

impl Plot {
    fn new(
        ui: &mut egui::Ui,
        h: f32,
        x: (f32, f32),
        y: (f32, f32),
        log_x: bool,
    ) -> (Plot, egui::Response) {
        let w = ui.available_width().max(200.0);
        let (rect, resp) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
        let p = ui.painter();
        p.rect_filled(rect, CornerRadius::ZERO, crate::theme::BG_DEEP());
        p.rect_stroke(
            rect,
            CornerRadius::ZERO,
            Stroke::new(1.0, crate::theme::LINE()),
            StrokeKind::Inside,
        );
        (Plot { rect, x, y, log_x }, resp)
    }

    fn fx(&self, x: f32) -> f32 {
        let t = if self.log_x {
            (x.ln() - self.x.0.ln()) / (self.x.1.ln() - self.x.0.ln())
        } else {
            (x - self.x.0) / (self.x.1 - self.x.0)
        };
        self.rect.left() + t.clamp(0.0, 1.0) * self.rect.width()
    }

    fn fy(&self, y: f32) -> f32 {
        let t = (y - self.y.0) / (self.y.1 - self.y.0);
        self.rect.bottom() - t.clamp(0.0, 1.0) * self.rect.height()
    }

    fn at(&self, x: f32, y: f32) -> Pos2 {
        pos2(self.fx(x), self.fy(y))
    }

    /// Inverse of [`Self::fx`] / [`Self::fy`], for dragging a handle.
    fn value_at(&self, p: Pos2) -> (f32, f32) {
        let tx = ((p.x - self.rect.left()) / self.rect.width()).clamp(0.0, 1.0);
        let ty = ((self.rect.bottom() - p.y) / self.rect.height()).clamp(0.0, 1.0);
        let x = if self.log_x {
            (self.x.0.ln() + tx * (self.x.1.ln() - self.x.0.ln())).exp()
        } else {
            self.x.0 + tx * (self.x.1 - self.x.0)
        };
        (x, self.y.0 + ty * (self.y.1 - self.y.0))
    }

    fn vline(&self, ui: &egui::Ui, x: f32, color: Color32, label: Option<&str>) {
        let px = self.fx(x);
        ui.painter().line_segment(
            [pos2(px, self.rect.top()), pos2(px, self.rect.bottom())],
            Stroke::new(1.0, color),
        );
        if let Some(l) = label {
            ui.painter().text(
                pos2(px + 2.0, self.rect.bottom() - 2.0),
                Align2::LEFT_BOTTOM,
                l,
                FontId::proportional(10.0),
                crate::theme::CYAN_DIM(),
            );
        }
    }

    fn hline(&self, ui: &egui::Ui, y: f32, color: Color32, label: Option<&str>) {
        let py = self.fy(y);
        ui.painter().line_segment(
            [pos2(self.rect.left(), py), pos2(self.rect.right(), py)],
            Stroke::new(1.0, color),
        );
        if let Some(l) = label {
            ui.painter().text(
                pos2(self.rect.left() + 2.0, py - 1.0),
                Align2::LEFT_BOTTOM,
                l,
                FontId::proportional(10.0),
                crate::theme::CYAN_DIM(),
            );
        }
    }

    fn curve(&self, ui: &egui::Ui, pts: Vec<Pos2>, color: Color32) {
        ui.painter().add(egui::Shape::line(pts, Stroke::new(2.0, color)));
    }
}

/// A level-in / level-out transfer curve, for the gate and the compressor:
/// `out(in)` drawn from -`span` to 0 dBFS, the unity diagonal behind it, and
/// the live input level as a dot on the curve.
fn transfer_plot(
    ui: &mut egui::Ui,
    span: f32,
    live_in: f32,
    marks: &[(f32, &str)],
    out: impl Fn(f32) -> f32,
) {
    let (plot, _) = Plot::new(ui, 170.0, (-span, 0.0), (-span, 0.0), false);
    let grid = crate::theme::LINE();
    let mut db = -span;
    while db < 0.0 {
        plot.vline(ui, db, grid, None);
        plot.hline(ui, db, grid, Some(&format!("{db:.0}")));
        db += 10.0;
    }
    plot.curve(ui, vec![plot.at(-span, -span), plot.at(0.0, 0.0)], crate::theme::LINE_LIT());
    for &(x, label) in marks {
        plot.vline(ui, x, crate::theme::YELLOW().gamma_multiply(0.6), Some(label));
    }
    let n = 160;
    let pts = (0..=n)
        .map(|i| {
            let x = -span + span * i as f32 / n as f32;
            plot.at(x, out(x))
        })
        .collect();
    plot.curve(ui, pts, crate::theme::CYAN());
    if live_in > -span {
        ui.painter().circle_filled(plot.at(live_in, out(live_in)), 4.0, crate::theme::GREEN());
    }
}

/// A horizontal bar for a level (dBFS, -60..0) or a gain reduction (dB, 0..-24).
fn bar(ui: &mut egui::Ui, label: &str, value: f32, range: (f32, f32), color: Color32, w: f32) {
    ui.horizontal(|ui| {
        ui.add_sized(vec2(34.0, 14.0), egui::Label::new(RichText::new(label).size(10.5).weak()));
        let (rect, _) = ui.allocate_exact_size(vec2(w, 10.0), Sense::hover());
        let p = ui.painter();
        p.rect_filled(rect, CornerRadius::ZERO, crate::theme::BG_DEEP());
        let t = ((value - range.0) / (range.1 - range.0)).clamp(0.0, 1.0);
        if t > 0.0 {
            let fill = Rect::from_min_size(rect.min, vec2(rect.width() * t, rect.height()));
            p.rect_filled(fill, CornerRadius::ZERO, color);
        }
        p.rect_stroke(
            rect,
            CornerRadius::ZERO,
            Stroke::new(1.0, crate::theme::LINE()),
            StrokeKind::Inside,
        );
        let text = if value <= -119.0 { "—".to_string() } else { format!("{value:.1} dB") };
        ui.label(RichText::new(text).monospace().size(10.5));
    });
}

fn gr_bar(ui: &mut egui::Ui, label: &str, gr: f32) {
    bar(ui, label, -gr, (0.0, 24.0), crate::theme::YELLOW(), 160.0);
}

// ── Pages ──────────────────────────────────────────────────────────────────

fn gate_page(ui: &mut egui::Ui, g: &mut TxGateState, m: TxVoiceMeters) {
    page_title(
        ui,
        "Gate / expander",
        "Pushes room noise, fan rumble and hiss down in the pauses. Set the threshold just \
         above your noise floor and below your quietest syllables.",
    );
    ui.horizontal(|ui| {
        let gate = g.ratio >= 9.9 && g.floor_db <= -39.9;
        let exp = (g.ratio - 2.0).abs() < 0.01 && (g.floor_db + 15.0).abs() < 0.01;
        if crate::chrome::chip(ui, gate, "GATE")
            .on_hover_text("Hard gate: 10:1, down to -40 dB")
            .clicked()
        {
            *g = g.as_gate();
        }
        if crate::chrome::chip(ui, exp, "EXPANDER")
            .on_hover_text(
                "Gentle expander: 2:1, never more than -15 dB — the room never goes dead",
            )
            .clicked()
        {
            *g = g.as_expander();
        }
        if crate::chrome::chip(ui, false, "RESET").on_hover_text("Back to the defaults").clicked() {
            *g = TxGateState { enabled: g.enabled, ..Default::default() };
        }
    });
    let cfg = *g;
    let out = move |x: f32| {
        if x >= cfg.threshold_db {
            x
        } else {
            x + ((x - cfg.threshold_db) * (cfg.ratio - 1.0)).max(cfg.floor_db)
        }
    };
    transfer_plot(
        ui,
        80.0,
        m.input_db,
        &[(g.threshold_db, "open"), (g.threshold_db - g.return_db, "close")],
        out,
    );
    egui::Grid::new("txa-gate").num_columns(2).spacing([10.0, 4.0]).show(ui, |ui| {
        knob(ui, "Threshold", "Below this the gate starts to close.", &mut g.threshold_db, TxGateState::THRESHOLD_DB_RANGE, " dB", false);
        knob(ui, "Return", "Hysteresis: once open, it closes only this far below the threshold, so a voice hovering at it does not make the gate flutter.", &mut g.return_db, TxGateState::RETURN_DB_RANGE, " dB", false);
        knob(ui, "Hold", "How long it stays open after the voice drops, so word endings are not clipped.", &mut g.hold_ms, TxGateState::HOLD_MS_RANGE, " ms", false);
        knob(ui, "Release", "How fast it closes once the hold is over. Lengthen it if the gate pumps.", &mut g.release_ms, TxGateState::RELEASE_MS_RANGE, " ms", true);
        knob(ui, "Floor", "The most it ever turns the pauses down.", &mut g.floor_db, TxGateState::FLOOR_DB_RANGE, " dB", false);
        knob(ui, "Ratio", "How steeply it closes below the threshold.", &mut g.ratio, TxGateState::RATIO_RANGE, ":1", false);
        knob(ui, "Look-ahead", "Opens this much before a word starts, so its first consonant is not chopped. Adds the same delay to everything transmitted.", &mut g.lookahead_ms, TxGateState::LOOKAHEAD_MS_RANGE, " ms", false);
    });
    gr_bar(ui, "GR", m.gate_gr_db);
}

/// The draggable bands on the EQ curve: (which, frequency, gain).
#[derive(Clone, Copy)]
enum EqHandle {
    Low,
    Mid,
    Mid2,
    High,
    Hpf,
    Lpf,
}

fn eq_page(ui: &mut egui::Ui, eq: &mut TxEqState) {
    page_title(
        ui,
        "EQ",
        "High-pass, low shelf, two peaks, high shelf and low-pass. Drag a point on the \
         curve, or set it below. Set the low-pass to your transmit filter's width.",
    );
    ui.horizontal(|ui| {
        if crate::chrome::chip(ui, false, "FLAT")
            .on_hover_text("Every band back to 0 dB, both edges off")
            .clicked()
        {
            *eq = TxEqState { enabled: eq.enabled, ..Default::default() };
        }
    });
    let (lo, hi) = (40.0f32, 10_000.0f32);
    let (plot, _) = Plot::new(ui, 190.0, (lo, hi), (-18.0, 18.0), true);
    for f in [100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0] {
        let label = if f >= 1000.0 { format!("{}k", f / 1000.0) } else { format!("{f}") };
        plot.vline(ui, f, crate::theme::LINE(), Some(&label));
    }
    for db in [-12.0, -6.0, 6.0, 12.0] {
        plot.hline(ui, db, crate::theme::LINE(), Some(&format!("{db:+.0}")));
    }
    plot.hline(ui, 0.0, crate::theme::LINE_LIT(), None);
    // The voice band an SSB transmitter carries.
    plot.vline(ui, 300.0, crate::theme::YELLOW().gamma_multiply(0.35), None);
    plot.vline(ui, 2700.0, crate::theme::YELLOW().gamma_multiply(0.35), Some("SSB"));

    let n = 240;
    let pts = (0..=n)
        .map(|i| {
            let f = (lo.ln() + (hi.ln() - lo.ln()) * i as f32 / n as f32).exp();
            plot.at(f, eq.response_db(CHAIN_RATE, f as f64) as f32)
        })
        .collect();
    let color = if eq.enabled { crate::theme::CYAN() } else { crate::theme::CYAN_DIM() };
    plot.curve(ui, pts, color);

    let handles = [
        (EqHandle::Hpf, eq.hpf.freq_hz, -3.0, eq.hpf.enabled, "HP"),
        (EqHandle::Low, eq.low.freq_hz, eq.low.gain_db, true, "L"),
        (EqHandle::Mid, eq.mid.freq_hz, eq.mid.gain_db, true, "1"),
        (EqHandle::Mid2, eq.mid2.freq_hz, eq.mid2.gain_db, true, "2"),
        (EqHandle::High, eq.high.freq_hz, eq.high.gain_db, true, "H"),
        (EqHandle::Lpf, eq.lpf.freq_hz, -3.0, eq.lpf.enabled, "LP"),
    ];
    for (i, (h, f, g, on, label)) in handles.into_iter().enumerate() {
        if !on {
            continue;
        }
        let at = plot.at(f, g);
        let rect = Rect::from_center_size(at, Vec2::splat(16.0));
        let resp = ui.interact(rect, ui.id().with(("txa-eq-handle", i)), Sense::drag());
        if resp.dragged()
            && let Some(p) = resp.interact_pointer_pos()
        {
            let (nf, ng) = plot.value_at(p);
            let band = |b: &mut TxEqBand, r: std::ops::RangeInclusive<f32>| {
                b.freq_hz = nf.clamp(*r.start(), *r.end()).round();
                b.gain_db = (ng * 2.0).round() / 2.0;
            };
            let edge = |e: &mut TxEqEdge, r: std::ops::RangeInclusive<f32>| {
                e.freq_hz = nf.clamp(*r.start(), *r.end()).round();
            };
            match h {
                EqHandle::Low => band(&mut eq.low, TxEqState::LOW_FREQ_HZ_RANGE),
                EqHandle::Mid => band(&mut eq.mid, TxEqState::MID_FREQ_HZ_RANGE),
                EqHandle::Mid2 => band(&mut eq.mid2, TxEqState::MID2_FREQ_HZ_RANGE),
                EqHandle::High => band(&mut eq.high, TxEqState::HIGH_FREQ_HZ_RANGE),
                EqHandle::Hpf => edge(&mut eq.hpf, TxEqState::HPF_HZ_RANGE),
                EqHandle::Lpf => edge(&mut eq.lpf, TxEqState::LPF_HZ_RANGE),
            }
            clamp_gains(eq);
        }
        let hot = resp.hovered() || resp.dragged();
        let fill = if hot { crate::theme::YELLOW() } else { color };
        ui.painter().circle_filled(at, 7.0, fill);
        ui.painter().text(
            at,
            Align2::CENTER_CENTER,
            label,
            FontId::proportional(9.0),
            crate::theme::INK_ON_BRIGHT(),
        );
        resp.on_hover_text(format!("{f:.0} Hz, {g:+.1} dB — drag to move"));
    }

    egui::Grid::new("txa-eq").num_columns(5).spacing([10.0, 4.0]).show(ui, |ui| {
        ui.label("");
        ui.label(RichText::new("Freq").weak());
        ui.label(RichText::new("Gain").weak());
        ui.label(RichText::new("Q / Slope").weak());
        ui.label("");
        ui.end_row();
        edge_row(
            ui,
            "High-pass",
            &mut eq.hpf,
            TxEqState::HPF_HZ_RANGE,
            "Removes handling noise, breath pops and hum below the corner.",
        );
        band_row(
            ui,
            "Low shelf",
            &mut eq.low,
            TxEqState::LOW_FREQ_HZ_RANGE,
            TxEqState::SHELF_SLOPE_RANGE,
            "Body, or mud: cut a little here for a voice that sounds boxy.",
        );
        band_row(
            ui,
            "Peak 1",
            &mut eq.mid,
            TxEqState::MID_FREQ_HZ_RANGE,
            TxEqState::MID_Q_RANGE,
            "A bump or dip in the voice band — often a cut around 300–500 Hz.",
        );
        band_row(
            ui,
            "Peak 2",
            &mut eq.mid2,
            TxEqState::MID2_FREQ_HZ_RANGE,
            TxEqState::MID_Q_RANGE,
            "Presence: a lift around 2–3 kHz is what cuts through on SSB.",
        );
        band_row(
            ui,
            "High shelf",
            &mut eq.high,
            TxEqState::HIGH_FREQ_HZ_RANGE,
            TxEqState::SHELF_SLOPE_RANGE,
            "Brightness.",
        );
        edge_row(
            ui,
            "Low-pass",
            &mut eq.lpf,
            TxEqState::LPF_HZ_RANGE,
            "Stops energy above the transmit filter from being spent in the modulator.",
        );
    });
}

/// Keep a dragged gain inside the range the controls offer.
fn clamp_gains(eq: &mut TxEqState) {
    let r = TxEqState::GAIN_DB_RANGE;
    for b in [&mut eq.low, &mut eq.mid, &mut eq.mid2, &mut eq.high] {
        b.gain_db = b.gain_db.clamp(*r.start(), *r.end());
    }
}

fn band_row(
    ui: &mut egui::Ui,
    label: &str,
    band: &mut TxEqBand,
    freq: std::ops::RangeInclusive<f32>,
    q: std::ops::RangeInclusive<f32>,
    hover: &str,
) {
    ui.label(label).on_hover_text(hover);
    ui.add(egui::DragValue::new(&mut band.freq_hz).range(freq).suffix(" Hz").speed(5.0));
    ui.add(
        egui::DragValue::new(&mut band.gain_db)
            .range(TxEqState::GAIN_DB_RANGE)
            .suffix(" dB")
            .speed(0.1),
    );
    ui.add(egui::DragValue::new(&mut band.q).range(q).speed(0.02).max_decimals(2));
    ui.label("");
    ui.end_row();
}

fn edge_row(
    ui: &mut egui::Ui,
    label: &str,
    e: &mut TxEqEdge,
    freq: std::ops::RangeInclusive<f32>,
    hover: &str,
) {
    ui.horizontal(|ui| {
        crate::chrome::checkbox(ui, &mut e.enabled, label).on_hover_text(hover);
    });
    ui.add_enabled(
        e.enabled,
        egui::DragValue::new(&mut e.freq_hz).range(freq).suffix(" Hz").speed(5.0),
    );
    ui.label("");
    ui.add_enabled_ui(e.enabled, |ui| {
        let text = if e.steep { "24 dB/oct" } else { "12 dB/oct" };
        if crate::chrome::chip(ui, e.steep, text)
            .on_hover_text("Click for a steeper or gentler slope")
            .clicked()
        {
            e.steep = !e.steep;
        }
    });
    ui.label("");
    ui.end_row();
}

fn deess_page(ui: &mut egui::Ui, d: &mut TxDeEssState, m: TxVoiceMeters) {
    page_title(
        ui,
        "De-esser",
        "Turns down only the sibilant band when \"s\" and \"sh\" cross the threshold, so the \
         rest of the voice keeps its level. Find the frequency by sweeping it while saying \
         \"sister silver cellophane\" and parking where the hiss is loudest.",
    );
    ui.horizontal(|ui| {
        if crate::chrome::chip(ui, false, "RESET").on_hover_text("Back to the defaults").clicked() {
            *d = TxDeEssState { enabled: d.enabled, ..Default::default() };
        }
    });
    egui::Grid::new("txa-deess").num_columns(2).spacing([10.0, 4.0]).show(ui, |ui| {
        knob(ui, "Frequency", "Centre of the sibilant band.", &mut d.freq_hz, TxDeEssState::FREQ_HZ_RANGE, " Hz", true);
        knob(ui, "Q", "Width of the band — higher is narrower.", &mut d.q, TxDeEssState::Q_RANGE, "", false);
        knob(ui, "Threshold", "Band level where reduction starts. Too low and it works all the time, dulling the voice.", &mut d.threshold_db, TxDeEssState::THRESHOLD_DB_RANGE, " dB", false);
        knob(ui, "Amount", "The most the band is ever turned down. Start around -6 dB.", &mut d.amount_db, TxDeEssState::AMOUNT_DB_RANGE, " dB", false);
        knob(ui, "Attack", "How fast it reacts.", &mut d.attack_ms, TxDeEssState::ATTACK_MS_RANGE, " ms", true);
        knob(ui, "Release", "How fast it lets go.", &mut d.release_ms, TxDeEssState::RELEASE_MS_RANGE, " ms", true);
    });
    gr_bar(ui, "GR", m.deess_gr_db);
}

fn comp_page(ui: &mut egui::Ui, c: &mut TxCompState, m: TxVoiceMeters) {
    page_title(
        ui,
        "Compressor",
        "Brings quiet syllables up and holds loud ones back. Aim for 3–6 dB of reduction on \
         peaks; more than 10 dB is obvious compression, better suited to contests.",
    );
    ui.horizontal(|ui| {
        if crate::chrome::chip(ui, false, "RESET").on_hover_text("Back to the defaults").clicked() {
            *c = TxCompState { enabled: c.enabled, ..Default::default() };
        }
    });
    let cfg = *c;
    transfer_plot(ui, 60.0, m.input_db, &[(c.threshold_db - c.drive_db, "threshold")], move |x| {
        x + cfg.drive_db + cfg.static_gain_db(x + cfg.drive_db) + cfg.makeup_db
    });
    egui::Grid::new("txa-comp").num_columns(2).spacing([10.0, 4.0]).show(ui, |ui| {
        knob(ui, "Threshold", "Level above which it starts to compress.", &mut c.threshold_db, TxCompState::THRESHOLD_DB_RANGE, " dB", false);
        knob(ui, "Ratio", "How hard it compresses above the threshold.", &mut c.ratio, TxCompState::RATIO_RANGE, ":1", true);
        knob(ui, "Attack", "How fast it reacts to a peak. 10–25 ms keeps consonants crisp.", &mut c.attack_ms, TxCompState::ATTACK_MS_RANGE, " ms", true);
        knob(ui, "Release", "How fast it recovers.", &mut c.release_ms, TxCompState::RELEASE_MS_RANGE, " ms", true);
        knob(ui, "Knee", "Soft-knee width; 0 is a hard knee.", &mut c.knee_db, TxCompState::KNEE_DB_RANGE, " dB", false);
        knob(ui, "Makeup", "Gain after compression, to bring the level back up.", &mut c.makeup_db, TxCompState::MAKEUP_DB_RANGE, " dB", false);
        knob(ui, "Drive", "Gain ahead of the compressor, so more of the voice crosses the threshold — raises the average power. Pair with the output limiter.", &mut c.drive_db, TxCompState::DRIVE_DB_RANGE, " dB", false);
        let mut stages = c.phase_stages as i32;
        ui.label("Phase").on_hover_text(
            "All-pass phase rotator ahead of the detector. Evens out the lopsided peaks a \
             voice has, so the same peak level carries more average power. 4 is a common \
             broadcast setting; 0 is off.",
        );
        if crate::chrome::slider_readout(ui, 78.0, Slider::new(&mut stages, 0..=TxCompState::PHASE_STAGES_MAX as i32))
            .changed()
        {
            c.phase_stages = stages.clamp(0, TxCompState::PHASE_STAGES_MAX as i32) as u8;
        }
        ui.end_row();
    });
    gr_bar(ui, "GR", m.comp_gr_db);
}

fn output_page(ui: &mut egui::Ui, o: &mut TxOutputState, m: TxVoiceMeters) {
    page_title(
        ui,
        "Output",
        "The last stage: a trim, and a look-ahead limiter that holds every peak under the \
         ceiling without the splatter hard clipping makes. Off by default, so switching the \
         chain on does not change a drive level you have already set.",
    );
    egui::Grid::new("txa-out").num_columns(2).spacing([10.0, 4.0]).show(ui, |ui| {
        ui.label("Limiter");
        crate::chrome::checkbox(ui, &mut o.limiter, "On");
        ui.end_row();
        knob(
            ui,
            "Ceiling",
            "No peak leaves the chain above this.",
            &mut o.ceiling_db,
            TxOutputState::CEILING_DB_RANGE,
            " dB",
            false,
        );
        knob(
            ui,
            "Trim",
            "Output gain, ahead of the limiter.",
            &mut o.trim_db,
            TxOutputState::TRIM_DB_RANGE,
            " dB",
            false,
        );
    });
    gr_bar(ui, "LIM", m.limiter_gr_db);
}

/// The meters along the foot of the window.
fn meter_strip(ui: &mut egui::Ui, v: &TxVoiceState, m: TxVoiceMeters) {
    ui.horizontal_wrapped(|ui| {
        ui.vertical(|ui| {
            bar(ui, "IN", m.input_db, (-60.0, 0.0), crate::theme::GREEN(), 200.0);
            let out_color =
                if m.output_db > -0.5 { crate::theme::ALERT() } else { crate::theme::CYAN() };
            bar(ui, "OUT", m.output_db, (-60.0, 0.0), out_color, 200.0);
        });
        ui.add_space(16.0);
        ui.vertical(|ui| {
            let gr = |on: bool, g: f32| if on { format!("{g:5.1}") } else { "  off".to_string() };
            ui.label(
                RichText::new(format!(
                    "GR  gate {}  de-ess {}  comp {}  lim {}",
                    gr(v.gate.enabled, m.gate_gr_db),
                    gr(v.deess.enabled, m.deess_gr_db),
                    gr(v.comp.enabled, m.comp_gr_db),
                    gr(v.output.limiter, m.limiter_gr_db),
                ))
                .monospace()
                .size(10.5),
            );
            ui.label(
                RichText::new(
                    "Meters move while you transmit a voice over or play the test phrase.",
                )
                .weak()
                .size(10.5),
            );
        });
    });
}

// ── Profiles ───────────────────────────────────────────────────────────────

/// Built-in starting points — tune from here, don't just load and transmit.
fn starting_points() -> Vec<(&'static str, TxVoiceState)> {
    let base = TxVoiceState::default();
    let dynamic_male = TxVoiceState {
        gate: TxGateState {
            enabled: true,
            threshold_db: -40.0,
            ratio: 2.5,
            release_ms: 80.0,
            floor_db: -15.0,
            ..Default::default()
        },
        eq: TxEqState {
            enabled: true,
            hpf: TxEqEdge { enabled: true, freq_hz: 80.0, steep: false },
            low: TxEqBand { freq_hz: 200.0, gain_db: -3.0, q: 0.7 },
            mid: TxEqBand { freq_hz: 2000.0, gain_db: 3.0, q: 1.0 },
            mid2: TxEqBand { freq_hz: 3500.0, gain_db: 1.0, q: 1.0 },
            lpf: TxEqEdge { enabled: true, freq_hz: 3100.0, steep: false },
            ..Default::default()
        },
        deess: TxDeEssState {
            enabled: true,
            freq_hz: 5000.0,
            q: 2.0,
            threshold_db: -25.0,
            amount_db: -6.0,
            ..Default::default()
        },
        comp: TxCompState {
            enabled: true,
            threshold_db: -20.0,
            ratio: 4.0,
            attack_ms: 25.0,
            release_ms: 150.0,
            makeup_db: 5.0,
            ..Default::default()
        },
        output: TxOutputState { limiter: true, ceiling_db: -1.0, trim_db: 0.0 },
        ..base
    };
    let condenser_female = TxVoiceState {
        gate: TxGateState {
            enabled: true,
            threshold_db: -45.0,
            ratio: 2.0,
            release_ms: 60.0,
            floor_db: -15.0,
            ..Default::default()
        },
        eq: TxEqState {
            enabled: true,
            hpf: TxEqEdge { enabled: true, freq_hz: 100.0, steep: false },
            mid: TxEqBand { freq_hz: 400.0, gain_db: -2.0, q: 1.0 },
            mid2: TxEqBand { freq_hz: 3000.0, gain_db: -2.0, q: 1.0 },
            high: TxEqBand { freq_hz: 4000.0, gain_db: 2.0, q: 0.7 },
            lpf: TxEqEdge { enabled: true, freq_hz: 3100.0, steep: false },
            ..Default::default()
        },
        deess: TxDeEssState {
            enabled: true,
            freq_hz: 7000.0,
            q: 2.5,
            threshold_db: -22.0,
            amount_db: -8.0,
            ..Default::default()
        },
        comp: TxCompState {
            enabled: true,
            threshold_db: -20.0,
            ratio: 3.0,
            attack_ms: 20.0,
            release_ms: 120.0,
            makeup_db: 4.0,
            ..Default::default()
        },
        output: TxOutputState { limiter: true, ceiling_db: -1.0, trim_db: 0.0 },
        ..base
    };
    let contest = TxVoiceState {
        gate: TxGateState {
            enabled: true,
            threshold_db: -35.0,
            release_ms: 100.0,
            ..Default::default()
        }
        .as_gate(),
        eq: TxEqState {
            enabled: true,
            hpf: TxEqEdge { enabled: true, freq_hz: 200.0, steep: true },
            mid: TxEqBand { freq_hz: 1000.0, gain_db: -3.0, q: 1.0 },
            mid2: TxEqBand { freq_hz: 2500.0, gain_db: 5.0, q: 1.0 },
            lpf: TxEqEdge { enabled: true, freq_hz: 2900.0, steep: true },
            ..Default::default()
        },
        deess: TxDeEssState {
            enabled: true,
            freq_hz: 5000.0,
            q: 2.0,
            threshold_db: -20.0,
            amount_db: -9.0,
            ..Default::default()
        },
        comp: TxCompState {
            enabled: true,
            threshold_db: -24.0,
            ratio: 8.0,
            attack_ms: 10.0,
            release_ms: 100.0,
            makeup_db: 8.0,
            drive_db: 6.0,
            phase_stages: 4,
            ..Default::default()
        },
        output: TxOutputState { limiter: true, ceiling_db: -1.0, trim_db: 0.0 },
        ..base
    };
    vec![
        ("Dynamic mic, male voice", dynamic_male),
        ("Condenser mic, female voice", condenser_female),
        ("Contest / DX punch", contest),
    ]
}

/// A profile file as exported: the profile itself, named.
fn export_profile(p: &TxVoiceProfile) {
    match serde_json::to_string_pretty(p) {
        Ok(json) => {
            let name = format!("{}.txaudio.json", p.name.replace(['/', '\\', ':'], "_"));
            crate::download::save(&name, json.as_bytes());
        }
        Err(e) => eprintln!("sdroxide: exporting TX audio profile: {e}"),
    }
}

/// Take a picked import file, if one has arrived, into the library.
fn pick_up_import(ui: &mut TxAudioUi) {
    let got = ui.import_inbox.lock().ok().and_then(|mut g| g.take());
    let Some(got) = got else { return };
    ui.note = Some(match got {
        Err(e) => e,
        Ok(loaded) => match serde_json::from_str::<TxVoiceProfile>(&loaded.text) {
            Err(e) => format!("Not a TX audio profile: {e}"),
            Ok(mut p) => {
                p.voice = p.voice.clamped();
                if p.name.trim().is_empty() {
                    p.name = "Imported".into();
                }
                let name = p.name.clone();
                match ui.profiles.iter().position(|q| q.name == p.name) {
                    Some(i) => ui.profiles[i] = p,
                    None => ui.profiles.push(p),
                }
                ui.save_profiles();
                format!("Imported \"{name}\".")
            }
        },
    });
}

fn profiles_page(ui: &mut egui::Ui, st: &mut TxAudioUi, v: &mut TxVoiceState) {
    page_title(
        ui,
        "Profiles",
        "A profile is the whole chain: every stage's settings, which are on, and their \
         order. Loading one replaces the chain; your microphone gain and drive stay as they \
         are.",
    );
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut st.profile_name).hint_text("Name").desired_width(180.0));
        let name = st.profile_name.trim().to_string();
        if ui.add_enabled(!name.is_empty(), egui::Button::new("Save")).on_hover_text("Store the chain as it is now under this name, replacing a profile of the same name").clicked() {
            let p = TxVoiceProfile { name: name.clone(), voice: *v };
            match st.profiles.iter().position(|q| q.name == name) {
                Some(i) => st.profiles[i] = p,
                None => st.profiles.push(p),
            }
            st.save_profiles();
            st.note = Some(format!("Saved \"{name}\"."));
        }
        if ui.button("Import…").on_hover_text("Read a profile from a .json file").clicked() {
            crate::download::load_text("TX audio profile", &["json"], st.import_inbox.clone());
        }
    });
    ui.add_space(4.0);
    let mut load: Option<TxVoiceState> = None;
    let mut delete: Option<usize> = None;
    if st.profiles.is_empty() {
        ui.label(
            RichText::new("No profiles yet. Save stores the chain you have set up now.").weak(),
        );
    }
    egui::Grid::new("txa-profiles").num_columns(4).spacing([8.0, 4.0]).striped(true).show(
        ui,
        |ui| {
            for (i, p) in st.profiles.iter().enumerate() {
                let sel = st.selected_profile == Some(i);
                let r = ui.selectable_label(sel, &p.name);
                if r.clicked() {
                    st.selected_profile = Some(i);
                    st.profile_name = p.name.clone();
                }
                if r.double_clicked() || ui.button("Load").clicked() {
                    load = Some(p.voice);
                }
                if ui.button("Export…").clicked() {
                    export_profile(p);
                }
                if ui.button("Delete").clicked() {
                    delete = Some(i);
                }
                ui.end_row();
            }
        },
    );
    if let Some(i) = delete {
        let p = st.profiles.remove(i);
        st.selected_profile = None;
        st.save_profiles();
        st.note = Some(format!("Deleted \"{}\".", p.name));
    }
    ui.add_space(8.0);
    ui.label(RichText::new("Starting points").strong());
    ui.label(
        RichText::new(
            "Tune from here rather than transmitting as loaded: record a phrase and listen \
             after each change.",
        )
        .weak()
        .size(11.0),
    );
    ui.horizontal_wrapped(|ui| {
        for (name, s) in starting_points() {
            if ui.button(name).clicked() {
                load = Some(s);
                st.note = Some(format!("Loaded the \"{name}\" starting point."));
            }
        }
    });
    if let Some(s) = load {
        // Keep the operator's own routing choices: a profile is about the
        // voice, not about whether digital voice goes through it.
        *v = TxVoiceState { digital_voice: v.digital_voice, ..s.clamped() };
        if st.note.is_none() {
            st.note = Some("Profile loaded.".into());
        }
    }
    if let Some(n) = &st.note {
        ui.add_space(4.0);
        ui.label(RichText::new(n).weak().size(11.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starting_points_are_valid_chains() {
        for (name, s) in starting_points() {
            assert_eq!(s, s.clamped(), "{name} has a value out of range");
            assert!(s.active(), "{name} does nothing");
        }
    }

    #[test]
    fn a_profile_round_trips_through_json() {
        let (name, voice) = starting_points().remove(2);
        let p = TxVoiceProfile { name: name.into(), voice };
        let back: TxVoiceProfile =
            serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn meters_fall_back_and_settle() {
        let mut b = Ballistics::default();
        let m = TxVoiceMeters { input_db: -6.0, comp_gr_db: -8.0, ..Default::default() };
        b.update(0.0, Some(m));
        assert_eq!(b.m.input_db, -6.0);
        b.update(0.1, None);
        assert!((b.m.input_db + 8.0).abs() < 1e-3);
        assert!((b.m.comp_gr_db + 6.0).abs() < 1e-3);
        for i in 0..100 {
            b.update(0.1 + i as f64 * 0.1, None);
        }
        assert!(!b.moving());
    }
}
