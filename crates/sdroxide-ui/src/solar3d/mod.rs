//! Solar-system 3D view: Sun, Earth and Moon with their orbits, the operator's
//! QTH on the globe, live SDO solar imagery, and DONKI sunspot / CME data.
//!
//! The scene, camera, overlay and renderer are shared by both targets. What
//! differs is only how the view is hosted and where its data comes from:
//!
//! * **Native** — [`Solar3d`] emits a deferred egui viewport, which desktop
//!   eframe turns into a real second OS window, and owns a [`SolarFeed`] that
//!   fetches from NASA and NOAA directly.
//!   [`SolarFeed`]: sdroxide_solar::SolarFeed
//! * **Browser** — [`SolarApp`] is the whole app of a second tab, and its data
//!   arrives over the server's `/solar-ws` relay. There is no viewport to
//!   manage: the browser owns the tab.
//!
//! Both end in the same [`overlay::ui`] call against the same [`SolarUi`], so
//! there is one implementation of the view itself.
//!
//! Unlike the rest of this crate, this module is *not* written to WebGL2
//! downlevel limits: it uses a depth buffer, MSAA and vertex buffers. All of it
//! is confined to an offscreen pass, so the shared egui render pass — which on
//! the web must stay within those limits — is untouched.

#[cfg(feature = "remote")]
mod app;
mod camera;
mod dotmatrix;
mod gpu;
mod math;
mod mesh;
#[cfg(feature = "remote")]
mod net;
mod overlay;
mod scene;
mod state;

// Everything below is the native host's: the browser's is in `app.rs`.
#[cfg(not(target_arch = "wasm32"))]
use std::sync::{Arc, Mutex, MutexGuard};

#[cfg(not(target_arch = "wasm32"))]
use eframe::egui;

#[cfg(not(target_arch = "wasm32"))]
use crate::egui_wgpu::RenderState;
#[cfg(not(target_arch = "wasm32"))]
use crate::view::Solar3dView;

#[cfg(feature = "remote")]
pub use app::SolarApp;
pub use state::DigiTraffic;
#[cfg(not(target_arch = "wasm32"))]
use state::SolarUi;

/// Stable id of the child viewport. Not a `const` because `Id::new` hashes at
/// runtime; it is a couple of instructions per frame.
#[cfg(not(target_arch = "wasm32"))]
fn viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("sdroxide-solar3d")
}

// Scene units are gigametres (10⁶ km): 1 AU ≈ 149.6, the Sun ≈ 0.696, the Earth
// ≈ 0.0064. Everything the camera deals with then sits in [1e-3, 1e3], well
// inside f32 range, while the ephemeris itself stays f64.
/// Earth mean radius, Gm.
pub const EARTH_R: f32 = 0.006_371;
/// Moon mean radius, Gm.
pub const MOON_R: f32 = 0.001_737_4;
/// Mean Earth–Moon distance, Gm.
pub const MOON_DIST: f32 = 0.384_4;

/// Largest body exaggeration that still keeps the Moon outside the Earth.
///
/// Radii are exaggerated but the Earth–Moon distance is not, so past this the
/// two spheres interpenetrate and the view becomes nonsense. Raising
/// `moon_orbit_scale` is what buys headroom.
pub fn max_body_scale(moon_orbit_scale: f32) -> f32 {
    0.45 * moon_orbit_scale * MOON_DIST / (EARTH_R + MOON_R)
}

/// App-side handle to the solar-system window.
///
/// Native only: it owns a [`sdroxide_solar::SolarFeed`] and a child viewport,
/// neither of which the browser build has. There, [`SolarApp`] is the host.
#[cfg(not(target_arch = "wasm32"))]
pub struct Solar3d {
    /// Whether the window should exist this frame. Toggled by the Display-box
    /// chip and cleared when the OS window is closed.
    pub open: bool,
    state: Arc<Mutex<SolarUi>>,
    /// Shared wgpu state, stashed so the GPU resources can be built on first
    /// open rather than at app construction — most sessions never open this
    /// window, and it allocates a few tens of MB.
    render_state: Option<RenderState>,
    gpu_ready: bool,
    /// The background data feed. Started when the window opens and **dropped
    /// when it closes**, which is what confines this feature's network activity
    /// to the window's lifetime: never opening it makes no request at all.
    feed: Option<sdroxide_solar::SolarFeed>,
    /// Channel/resolution the running feed was started with, so a change in the
    /// UI can be forwarded rather than restarting the thread.
    feed_channel: (u8, u16),
    /// The custom element sets the running feed was last told about. SGP4
    /// constants are not free to rebuild, so the listing is only re-sent when
    /// the operator actually changes it.
    feed_tles: String,
    /// Likewise for the subscribed listings: re-sending these would refetch
    /// nothing (the cache decides that) but would still reparse every one.
    feed_subs: Vec<sdroxide_types::TleSubscription>,
}

#[cfg(not(target_arch = "wasm32"))]
impl Solar3d {
    pub fn new(render_state: Option<RenderState>, view: Solar3dView) -> Self {
        Solar3d {
            open: view.open,
            state: Arc::new(Mutex::new(SolarUi::new(view))),
            render_state,
            gpu_ready: false,
            feed: None,
            feed_channel: (view.channel, view.resolution),
            feed_tles: String::new(),
            feed_subs: Vec::new(),
        }
    }

    /// Lock the shared state, recovering from a poisoned mutex: a panic inside
    /// the render closure should not turn into a panic on every later frame.
    fn lock(&self) -> MutexGuard<'_, SolarUi> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The settings to persist in `ViewState`.
    pub fn persisted(&self) -> Solar3dView {
        let mut v = self.lock().view;
        v.open = self.open;
        v
    }

    /// Whether the clouds are marched rather than sliced. Lives here rather than
    /// in `UiSettings` because it rides `Solar3dView`, which is the state the
    /// browser tab persists too — the same switch has to reach both.
    pub fn cloud_march(&self) -> bool {
        self.lock().view.cloud_march
    }

    pub fn set_cloud_march(&self, on: bool) {
        self.lock().view.cloud_march = on;
    }

    /// Emit the window for this frame (or not, when closed). Call once per root
    /// pass, after the main UI. `grid` is the operator's Maidenhead locator and
    /// `awards` the log's DXCC coverage for the "what is still missing" layer.
    pub fn viewport(
        &mut self,
        ctx: &egui::Context,
        grid: &str,
        traffic: DigiTraffic,
        awards: Arc<Vec<sdroxide_types::EntitySlot>>,
        sat_cfg: Arc<sdroxide_types::SatConfig>,
    ) {
        if !self.open {
            // Dropping the feed disconnects the worker's channel, which is how
            // it learns to stop. Closing the window therefore ends all network
            // activity, which is the behaviour the manual promises.
            if self.feed.take().is_some() {
                self.lock().data = None;
                // The next feed is a new thread that knows nothing, so forget
                // what the old one was told or the custom sets would never be
                // re-sent after a close and reopen.
                self.feed_tles.clear();
                self.feed_subs.clear();
            }
            return;
        }

        if !self.gpu_ready {
            if let Some(rs) = &self.render_state {
                gpu::init(rs);
            }
            self.gpu_ready = true;
        }
        self.ensure_feed(ctx);
        self.ensure_custom_tles(&sat_cfg);

        // Publish this frame's inputs, then drop the guard: the deferred
        // callback takes the same lock, and on the embedded-viewport path it
        // would run synchronously inside `show_viewport_deferred`.
        {
            let mut st = self.lock();
            st.set_qth(grid);
            st.digi = traffic;
            st.awards = awards;
            st.sat_cfg = sat_cfg;
        }

        let state = Arc::clone(&self.state);
        ctx.show_viewport_deferred(
            viewport_id(),
            egui::ViewportBuilder::default()
                .with_title("sdroxide — solar system")
                .with_inner_size([1180.0, 760.0])
                .with_min_inner_size([520.0, 340.0]),
            move |ui, _class| {
                let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                if ui.ctx().input(|i| i.viewport().close_requested()) {
                    st.close_requested = true;
                    // Wake the root pass so the chip un-lights promptly.
                    ui.ctx().request_repaint_of(egui::ViewportId::ROOT);
                    return;
                }
                overlay::ui(ui, &mut st);
            },
        );

        // egui tears the window down when we stop emitting the viewport — do
        // *not* send `ViewportCommand::Close` as well.
        let (close, refresh) = {
            let mut st = self.lock();
            (std::mem::take(&mut st.close_requested), std::mem::take(&mut st.refresh_requested))
        };
        if close {
            self.open = false;
        }
        if refresh {
            self.refresh();
        }
    }

    /// Start the data feed on first open, and forward channel/resolution
    /// changes to the running one rather than restarting the thread.
    fn ensure_feed(&mut self, ctx: &egui::Context) {
        let (channel, resolution) = {
            let st = self.lock();
            (st.view.channel, st.view.resolution)
        };

        if self.feed.is_none() {
            let wake_ctx = ctx.clone();
            let feed = sdroxide_solar::SolarFeed::start(
                sdroxide_solar::SdoChannel::from_u8(channel),
                resolution as u32,
                // Wake only this window: the SDR UI has no reason to redraw
                // because a solar image arrived.
                move || wake_ctx.request_repaint_of(viewport_id()),
            );
            self.lock().data = Some(feed.shared());
            self.feed = Some(feed);
            self.feed_channel = (channel, resolution);
            return;
        }

        if self.feed_channel != (channel, resolution) {
            if let Some(feed) = &self.feed {
                if self.feed_channel.0 != channel {
                    feed.send(sdroxide_solar::FeedCmd::SetChannel(
                        sdroxide_solar::SdoChannel::from_u8(channel),
                    ));
                }
                if self.feed_channel.1 != resolution {
                    feed.send(sdroxide_solar::FeedCmd::SetResolution(resolution as u32));
                }
            }
            self.feed_channel = (channel, resolution);
        }
    }

    /// Push the operator's own element sets into the feed when they change.
    ///
    /// The listing is compared rather than the config, so editing a frequency
    /// entry — which the tracker does not care about — costs nothing.
    fn ensure_custom_tles(&mut self, cfg: &sdroxide_types::SatConfig) {
        let Some(feed) = &self.feed else { return };
        let text = cfg.tle_text();
        if text != self.feed_tles {
            feed.send(sdroxide_solar::FeedCmd::SetCustomTles(text.clone()));
            self.feed_tles = text;
        }
        let subs: Vec<_> = cfg.live_subs().cloned().collect();
        if subs != self.feed_subs {
            feed.send(sdroxide_solar::FeedCmd::SetTleSubs(subs.clone()));
            self.feed_subs = subs;
        }
    }

    /// What each TLE subscription's last fetch did, for the settings dialog.
    ///
    /// Empty while the window has never been open this session: the feed — and
    /// with it every fetch — only exists while the window does. The settings
    /// dialog falls back to reading the disk cache in that case, so a
    /// subscription still reports its age with the window shut.
    pub fn tle_sub_status(&self) -> Vec<sdroxide_solar::SubStatus> {
        let st = self.lock();
        let Some(data) = &st.data else { return Vec::new() };
        data.lock().unwrap_or_else(|e| e.into_inner()).tle_subs.clone()
    }

    /// Re-read the subscribed listings from the disk cache.
    ///
    /// Used after the settings dialog fetches them itself: the feed shares the
    /// cache but has no way to know somebody else just wrote to it, so without
    /// this the window would keep showing the previous listing until its own
    /// six-hourly refresh came round.
    pub fn reload_tle_subs(&mut self) {
        if let Some(feed) = &self.feed {
            feed.send(sdroxide_solar::FeedCmd::SetTleSubs(self.feed_subs.clone()));
        }
    }

    /// Ask the feed to re-fetch everything now (the overlay's ↻ button).
    pub fn refresh(&self) {
        if let Some(feed) = &self.feed {
            feed.send(sdroxide_solar::FeedCmd::RefreshAll);
        }
    }
}

/// Seconds since the Unix epoch. The ephemeris takes an explicit timestamp
/// everywhere, so the whole scene can be scrubbed by offsetting this.
fn wall_clock_unix() -> f64 {
    crate::time::now_unix_f64()
}
