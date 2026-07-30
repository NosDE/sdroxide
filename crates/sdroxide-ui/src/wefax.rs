//! Client-side state for the weather-fax panel: the chart being painted, and
//! the gallery of ones already saved.
//!
//! The picture is built here as well as in the engine. That is not duplication
//! for its own sake — a remote client sees only the line events, never the
//! engine's buffer, and the panel has to be able to paint a chart that is still
//! twelve minutes from finishing.

use eframe::egui;

/// Rows the live chart grows by. A chart is 1809 pixels wide, so this is about
/// half a megabyte at a time rather than a reallocation per scan line.
const GROW_ROWS: usize = 256;

/// Widest a chart may get before the panel refuses it, as a guard on a length
/// that arrives from the wire.
const MAX_W: usize = 4096;
/// Tallest, matching the demodulator's own limit with headroom.
const MAX_H: usize = 4096;

/// Charts held as textures at once. A chart is two megapixels, so a season of
/// them would be gigabytes of VRAM; the rest stay on disk and are counted
/// rather than loaded.
pub const GALLERY_MAX: usize = 48;

/// Magnification limits for the live view. The lower end is enough to see a
/// whole chart's layout at a glance; the upper end is where a fax pixel is
/// four screen pixels, past which there is no more detail to find.
pub const MIN_ZOOM: f32 = 0.1;
pub const MAX_ZOOM: f32 = 4.0;

/// How far the picture may be stretched or squashed vertically. Wide enough to
/// straighten a chart received at the wrong line rate — 60 against 120 is a
/// factor of two, and taking the wrong one of a pair of adjacent rates is the
/// usual way to end up with a chart twice as tall as it should be.
pub const MIN_ASPECT: f32 = 0.25;
pub const MAX_ASPECT: f32 = 4.0;

/// How the live chart is scaled into the panel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Zoom {
    /// Scale to the panel width, never past one screen pixel per fax pixel.
    FitWidth,
    /// Scale so the whole chart is in view at once, however tall it has grown.
    Whole,
    /// A fixed magnification, in screen pixels per fax pixel.
    Fixed(f32),
}

/// A saved chart in the gallery.
pub struct Chart {
    pub texture: egui::TextureHandle,
    /// File name it was saved under, which carries its date and dial.
    pub name: String,
    /// When it was received and where from, read back out of that name. `None`
    /// for a file in the directory that is not one of ours.
    pub meta: Option<sdroxide_types::WefaxChartMeta>,
    pub size: (u16, u16),
}

impl Chart {
    /// When it was received, for ordering. A chart whose name says nothing
    /// sorts oldest; there is nowhere better to put it.
    #[allow(dead_code)] // ordering is a disk-load concern, and wasm has no disk
    pub fn when(&self) -> i64 {
        self.meta.map_or(0, |m| m.unix)
    }

    /// Date, time and station, or the bare file name when the name is not one
    /// this program wrote.
    pub fn title(&self) -> String {
        self.meta.map_or_else(|| self.name.clone(), |m| m.label())
    }
}

pub struct WefaxUi {
    /// Engine status, as of the last update.
    pub status: sdroxide_types::WefaxStatus,
    /// The chart being received: one byte per pixel, top row first.
    live: Vec<u8>,
    live_w: u16,
    live_h: u16,
    /// Which picture `live` belongs to, so a restarted transmission clears it
    /// rather than appending to the previous chart.
    image_id: u32,
    /// The live chart as a texture, rebuilt when rows have arrived.
    live_tex: Option<egui::TextureHandle>,
    dirty: bool,
    /// How the live chart is scaled into the panel.
    pub zoom: Zoom,
    /// Extra vertical scaling of the live chart, on top of `zoom`.
    ///
    /// Its own control because the two things that make a chart the wrong shape
    /// are different problems: the *size* on screen is a display preference,
    /// while the *proportions* are a property of the signal — a chart received
    /// at the wrong line rate comes out stretched or squashed, and being able to
    /// pull it back into shape is what makes it readable before the operator has
    /// worked out which rate the station is actually using.
    pub aspect: f32,
    /// Whether the live view keeps the newest rows in sight. The operator
    /// scrolling up turns it off — reading the top of a chart while the bottom
    /// is still arriving is the whole point of being able to scroll — and
    /// scrolling back to the bottom turns it on again.
    pub follow: bool,
    /// Saved charts, newest first.
    pub gallery: Vec<Chart>,
    pub loaded_disk: bool,
    /// Charts on disk beyond the ones held as textures, so the panel can say
    /// that the gallery is not the whole collection.
    pub disk_extra: usize,
    /// Where charts are being saved, for the panel to show and to copy.
    pub dir: String,
    /// Which gallery entry is open full-size, if any.
    pub viewing: Option<usize>,
}

impl Default for WefaxUi {
    fn default() -> Self {
        WefaxUi {
            status: Default::default(),
            live: Vec::new(),
            live_w: 0,
            live_h: 0,
            image_id: 0,
            live_tex: None,
            dirty: false,
            // Fitted, unstretched and following: what an operator wants before
            // they have decided to go looking at anything in particular.
            zoom: Zoom::FitWidth,
            aspect: 1.0,
            follow: true,
            gallery: Vec::new(),
            loaded_disk: false,
            disk_extra: 0,
            dir: String::new(),
            viewing: None,
        }
    }
}

impl WefaxUi {
    /// Adopt a freshly decoded scan line.
    pub fn push_line(&mut self, image_id: u32, y: u16, gray: &[u8]) {
        if gray.is_empty() || gray.len() > MAX_W {
            return;
        }
        // A new id, or a row before the write head, means a new transmission.
        if image_id != self.image_id || y == 0 {
            self.image_id = image_id;
            self.live.clear();
            self.live_w = gray.len() as u16;
            self.live_h = 0;
        }
        if gray.len() != self.live_w as usize || self.live_h as usize >= MAX_H {
            return;
        }
        // Rows arrive in order. A gap means lines were dropped somewhere
        // upstream; fill it with mid-grey rather than sliding the rest of the
        // chart up, which would put a seam through the picture instead of a
        // band and be far harder to see.
        while (self.live_h as usize) < y as usize {
            self.live.extend(std::iter::repeat_n(128u8, self.live_w as usize));
            self.live_h += 1;
        }
        if y as usize != self.live_h as usize {
            return;
        }
        if self.live.capacity() < self.live.len() + self.live_w as usize {
            self.live.reserve(self.live_w as usize * GROW_ROWS);
        }
        self.live.extend_from_slice(gray);
        self.live_h += 1;
        self.dirty = true;
    }

    /// Rows received for the chart in progress.
    pub fn live_size(&self) -> (u16, u16) {
        (self.live_w, self.live_h)
    }

    pub fn has_live(&self) -> bool {
        self.live_h > 0
    }

    /// Throw the live chart away — the operator restarting, or a completed one
    /// having moved to the gallery.
    pub fn clear_live(&mut self) {
        self.live.clear();
        self.live_h = 0;
        self.live_tex = None;
        self.dirty = false;
    }

    /// The live chart as a texture, rebuilt only when rows have arrived.
    ///
    /// A full chart is two megapixels and re-uploading it every frame at 120
    /// lines a minute would spend the whole GPU budget on a picture that
    /// changes twice a second.
    pub fn live_texture(&mut self, ctx: &egui::Context) -> Option<&egui::TextureHandle> {
        if self.live_h == 0 {
            return None;
        }
        if self.dirty || self.live_tex.is_none() {
            let img = gray_image(&self.live, self.live_w, self.live_h);
            match &mut self.live_tex {
                Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                None => {
                    self.live_tex =
                        Some(ctx.load_texture("wefax-live", img, egui::TextureOptions::LINEAR))
                }
            }
            self.dirty = false;
        }
        self.live_tex.as_ref()
    }

    /// Add a decoded PNG to the front of the gallery.
    ///
    /// The name is the metadata: when the chart was received and what it was
    /// tuned to, both read straight back out of it, so a chart loaded from disk
    /// is labelled exactly like one that has just arrived.
    pub fn add_chart(&mut self, ctx: &egui::Context, name: &str, png: &[u8]) {
        let Some((gray, w, h)) = decode_gray(png) else { return };
        let texture = ctx.load_texture(
            format!("wefax-{name}"),
            gray_image(&gray, w, h),
            egui::TextureOptions::LINEAR,
        );
        self.gallery.insert(
            0,
            Chart {
                texture,
                name: name.to_string(),
                meta: sdroxide_types::WefaxChartMeta::from_file_name(name),
                size: (w, h),
            },
        );
        // Textures dropped off the end are still on disk, and saying so is the
        // difference between a gallery that forgets and one that is a window
        // onto a directory.
        if self.gallery.len() > GALLERY_MAX {
            self.disk_extra += self.gallery.len() - GALLERY_MAX;
            self.gallery.truncate(GALLERY_MAX);
        }
        // The entry the viewer is on has just moved down by one.
        if let Some(v) = self.viewing.as_mut() {
            *v += 1;
        }
    }

    /// Put the gallery in newest-first order, whatever order the entries
    /// arrived in.
    #[allow(dead_code)] // only the disk load can produce an out-of-order gallery
    pub fn sort_gallery(&mut self) {
        self.gallery.sort_by(|a, b| b.when().cmp(&a.when()).then_with(|| a.name.cmp(&b.name)));
    }
}

/// The size on screen of a chart `size` pixels across, shown in a `view`-sized
/// area at magnification `zoom` and vertical stretch `aspect`.
pub fn live_size(zoom: Zoom, aspect: f32, view: (f32, f32), size: (u16, u16)) -> (f32, f32) {
    let (w, h) = (size.0.max(1) as f32, size.1.max(1) as f32);
    let aspect = if aspect.is_finite() { aspect.clamp(MIN_ASPECT, MAX_ASPECT) } else { 1.0 };
    let scale = match zoom {
        // Fit — but never blow a chart up past its own pixels. A 904-pixel
        // IOC 288 chart stretched across a wide panel is a blurrier picture,
        // not a bigger one, and magnification can be asked for.
        Zoom::FitWidth => (view.0 / w).clamp(MIN_ZOOM, 1.0),
        // No lower limit worth the name: a finished chart is two thousand lines
        // tall in a panel a few hundred high, and a floor that stopped short of
        // fitting it would make this the same button as FIT.
        Zoom::Whole => (view.0 / w).min(view.1 / (h * aspect)).clamp(0.01, 1.0),
        Zoom::Fixed(z) => {
            if z.is_finite() {
                z.clamp(MIN_ZOOM, MAX_ZOOM)
            } else {
                1.0
            }
        }
    };
    (w * scale, h * scale * aspect)
}

/// Decode a PNG to a single-channel raster plus its size.
pub fn decode_gray(png: &[u8]) -> Option<(Vec<u8>, u16, u16)> {
    let img = image::load_from_memory(png).ok()?.to_luma8();
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 || w as usize > MAX_W * 4 || h as usize > MAX_H * 4 {
        return None;
    }
    Some((img.into_raw(), w as u16, h as u16))
}

/// A single-channel raster as an egui image.
pub fn gray_image(gray: &[u8], w: u16, h: u16) -> egui::ColorImage {
    let (w, h) = (w as usize, h as usize);
    let mut px = Vec::with_capacity(w * h);
    for i in 0..w * h {
        let v = gray.get(i).copied().unwrap_or(0);
        px.push(egui::Color32::from_gray(v));
    }
    egui::ColorImage { size: [w, h], pixels: px, source_size: egui::vec2(w as f32, h as f32) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_build_the_chart_downwards() {
        let mut ui = WefaxUi::default();
        for y in 0..4u16 {
            ui.push_line(1, y, &[y as u8; 8]);
        }
        assert_eq!(ui.live_size(), (8, 4));
        assert_eq!(ui.live[0], 0);
        assert_eq!(ui.live[8], 1);
        assert_eq!(ui.live[24], 3);
        assert!(ui.has_live());
    }

    /// A gap in the rows has to leave a band, not slide the rest of the chart
    /// up — a seam through the middle of a weather map is much harder to spot
    /// than a grey stripe, and it puts every isobar below it in the wrong place.
    #[test]
    fn a_dropped_row_leaves_a_band_rather_than_a_seam() {
        let mut ui = WefaxUi::default();
        ui.push_line(1, 0, &[10; 4]);
        ui.push_line(1, 3, &[20; 4]);
        assert_eq!(ui.live_size(), (4, 4));
        assert_eq!(ui.live[0], 10);
        assert_eq!(&ui.live[4..12], &[128; 8], "the gap should be mid-grey");
        assert_eq!(ui.live[12], 20);
    }

    /// A new transmission starts a new picture rather than appending to the
    /// last one.
    #[test]
    fn a_new_transmission_starts_a_new_chart() {
        let mut ui = WefaxUi::default();
        ui.push_line(1, 0, &[1; 4]);
        ui.push_line(1, 1, &[2; 4]);
        assert_eq!(ui.live_size(), (4, 2));
        ui.push_line(2, 0, &[3; 6]);
        assert_eq!(ui.live_size(), (6, 1), "the width should follow the new picture");
        assert_eq!(ui.live[0], 3);
        ui.clear_live();
        assert!(!ui.has_live());
        assert_eq!(ui.live_size(), (6, 0));
    }

    /// Rubbish off the wire must be refused rather than sized into an
    /// allocation, and a row that does not match the picture's width dropped.
    #[test]
    fn malformed_rows_are_refused() {
        let mut ui = WefaxUi::default();
        ui.push_line(1, 0, &[]);
        assert!(!ui.has_live());
        ui.push_line(1, 0, &vec![0u8; MAX_W + 1]);
        assert!(!ui.has_live());
        // A width change mid-picture is not a picture.
        ui.push_line(1, 0, &[1; 8]);
        ui.push_line(1, 1, &[1; 9]);
        assert_eq!(ui.live_size(), (8, 1));
        // A row behind the write head is dropped rather than overwriting.
        ui.push_line(1, 5, &[2; 8]);
        assert_eq!(ui.live_size(), (8, 6), "the gap is filled forwards");
    }

    /// Fitting must never magnify — a chart blown up past its own pixels is
    /// blurrier, not more legible — and an explicit magnification must be
    /// honoured whatever the panel is doing, since that is what makes the
    /// isobars readable while the chart is still arriving.
    #[test]
    fn fitting_shrinks_to_the_panel_and_magnifying_ignores_it() {
        // A 1809-pixel chart in a 400-pixel panel fits at a little over a fifth.
        let (w, h) = live_size(Zoom::FitWidth, 1.0, (400.0, 300.0), (1809, 1200));
        assert!((w - 400.0).abs() < 1e-3, "{w}");
        assert!((h - 1200.0 * 400.0 / 1809.0).abs() < 1e-3, "{h}");
        // A narrow chart in a wide panel stays at its own size.
        assert_eq!(live_size(Zoom::FitWidth, 1.0, (2000.0, 900.0), (904, 500)), (904.0, 500.0));
        // Magnification is taken as given, and clamped to something sane.
        assert_eq!(live_size(Zoom::Fixed(1.0), 1.0, (400.0, 300.0), (1809, 100)).0, 1809.0);
        assert_eq!(live_size(Zoom::Fixed(2.0), 1.0, (400.0, 300.0), (1809, 100)).0, 3618.0);
        assert_eq!(
            live_size(Zoom::Fixed(99.0), 1.0, (400.0, 300.0), (100, 10)).0,
            100.0 * MAX_ZOOM
        );
        assert_eq!(live_size(Zoom::Fixed(0.0), 1.0, (400.0, 300.0), (100, 10)).0, 100.0 * MIN_ZOOM);
        // A zero-sized picture must not divide by zero.
        let (w, h) = live_size(Zoom::FitWidth, 1.0, (400.0, 300.0), (0, 0));
        assert!(w.is_finite() && h.is_finite());
    }

    /// "Whole" has to bring a chart taller than the panel entirely into view —
    /// that is the difference between it and fitting the width, and the reason
    /// a finished chart can be taken in at a glance.
    #[test]
    fn the_whole_chart_fits_in_the_panel_both_ways() {
        // 1809 × 1200 in a 400 × 200 panel: width-fitting is 400 × 265, too
        // tall; whole shrinks until the height fits.
        let (_, h) = live_size(Zoom::FitWidth, 1.0, (400.0, 200.0), (1809, 1200));
        assert!(h > 200.0, "width-fitting leaves it too tall: {h}");
        let (w, h) = live_size(Zoom::Whole, 1.0, (400.0, 200.0), (1809, 1200));
        assert!(w <= 400.001 && h <= 200.001, "{w} × {h} should fit");
        // The aspect stretch is part of what has to fit, or a stretched chart
        // would still hang out of the bottom of the panel.
        let (w, h) = live_size(Zoom::Whole, 2.0, (400.0, 200.0), (1809, 1200));
        assert!(w <= 400.001 && h <= 200.001, "stretched: {w} × {h}");
    }

    /// The vertical stretch is what pulls a chart received at the wrong line
    /// rate back into shape, so it has to scale the height and nothing else.
    #[test]
    fn the_aspect_control_stretches_only_the_height() {
        let (w1, h1) = live_size(Zoom::Fixed(1.0), 1.0, (400.0, 300.0), (900, 200));
        let (w2, h2) = live_size(Zoom::Fixed(1.0), 2.0, (400.0, 300.0), (900, 200));
        assert_eq!(w1, w2, "the width must not move");
        assert_eq!((h1, h2), (200.0, 400.0));
        // Squashing works the same way, and both ends are clamped.
        assert_eq!(live_size(Zoom::Fixed(1.0), 0.5, (400.0, 300.0), (900, 200)).1, 100.0);
        assert_eq!(
            live_size(Zoom::Fixed(1.0), 99.0, (400.0, 300.0), (900, 200)).1,
            200.0 * MAX_ASPECT
        );
        assert_eq!(
            live_size(Zoom::Fixed(1.0), 0.0, (400.0, 300.0), (900, 200)).1,
            200.0 * MIN_ASPECT
        );
        // Rubbish must not produce a NaN-sized widget.
        let (w, h) = live_size(Zoom::Fixed(f32::NAN), f32::NAN, (400.0, 300.0), (900, 200));
        assert!(w.is_finite() && h.is_finite());
    }

    /// The gallery is browsed newest first, and the ordering has to come from
    /// when a chart was received rather than the order files happened to be
    /// read off disk.
    #[test]
    fn charts_are_ordered_newest_first_by_their_own_timestamps() {
        use sdroxide_types::WefaxChartMeta;
        let names = [
            "wefax-20260729-141530Z-7878.1kHz-DWD.png",
            "wefax-20260729-061500Z-3853.1kHz-DWD.png",
            "wefax-20260728-235900Z.png",
        ];
        let mut metas: Vec<_> =
            names.iter().map(|n| WefaxChartMeta::from_file_name(n).expect(n)).collect();
        metas.sort_by(|a, b| b.unix.cmp(&a.unix));
        assert_eq!(metas[0].when_label(), "2026-07-29 14:15Z");
        assert_eq!(metas[2].when_label(), "2026-07-28 23:59Z");
        // The newest of them names its station rather than a bare frequency.
        assert!(metas[0].where_label().unwrap().contains("DWD"));
    }

    #[test]
    fn a_raster_becomes_a_grey_image_of_the_right_size() {
        let img = gray_image(&[0, 128, 255, 64], 2, 2);
        assert_eq!(img.size, [2, 2]);
        assert_eq!(img.pixels[0], egui::Color32::from_gray(0));
        assert_eq!(img.pixels[2], egui::Color32::from_gray(255));
        // A short buffer is padded rather than panicking.
        assert_eq!(gray_image(&[1], 2, 2).pixels.len(), 4);
    }
}
