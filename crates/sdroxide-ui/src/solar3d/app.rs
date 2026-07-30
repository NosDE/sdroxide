//! The solar-system view as a whole application, for the browser tab that the
//! ☀ 3D chip opens.
//!
//! Natively this view is a child viewport of the main window and `Solar3d`
//! drives it from the root pass. In the browser there is no such parent: the
//! tab is its own wasm instance with its own canvas and its own wgpu device.
//! That turns out to be an advantage — the device is free to carry depth and
//! MSAA without the waterfall pipelines in the same process having to match.
//!
//! What this type does is exactly what `Solar3d::viewport` does per frame,
//! minus the viewport: drain the data source, publish the operator's QTH and
//! QSO traffic into [`SolarUi`], call [`overlay::ui`], and forward whatever the
//! overlay asked for back to the data source.

use eframe::egui;

use sdroxide_proto::solar::SolarClientMsg;

use super::net::{Link, SolarClient};
use super::overlay;
use super::state::SolarUi;
use crate::digi_map::DigiStations;
use crate::view::Solar3dView;

/// Storage key for the persisted camera/layer/scale settings.
///
/// Deliberately not `"view"`: that is the main page's whole [`crate::view::ViewState`],
/// and the two tabs share an origin. Same-named keys would have them overwrite
/// each other's settings on every save.
const STORAGE_KEY: &str = "solar3d";

pub struct SolarApp {
    state: SolarUi,
    net: SolarClient,
    stations: DigiStations,
    /// Reported once, so a failed connection does not spam the log every frame.
    reported: Option<Link>,
}

impl SolarApp {
    /// `url` is the `/solar-ws` endpoint. A connection failure is not fatal:
    /// the ephemeris is arithmetic, so the Sun, the planets and the operator's
    /// own QTH still draw. Only the live products go missing, and the overlay's
    /// freshness readouts already say so.
    pub fn new(cc: &eframe::CreationContext<'_>, url: &str) -> Result<Self, String> {
        crate::theme::apply(&cc.egui_ctx);
        if let Some(rs) = cc.wgpu_render_state.as_ref() {
            super::gpu::init(rs);
        }

        let view: Solar3dView =
            cc.storage.and_then(|s| eframe::get_value(s, STORAGE_KEY)).unwrap_or_default();

        let ctx = cc.egui_ctx.clone();
        let net = SolarClient::connect(url, move || {
            ctx.request_repaint_after(std::time::Duration::from_millis(33))
        })?;

        let mut state = SolarUi::new(view);
        state.data = Some(net.shared());
        Ok(SolarApp { state, net, stations: DigiStations::default(), reported: None })
    }
}

impl eframe::App for SolarApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.net.poll();
        // Report each transition once, not every frame. The overlay already
        // tells the honest story on its own — with no link, no source ever
        // reports a successful fetch and every freshness readout says so — so
        // this is for the console, not for the operator.
        if self.reported.as_ref() != Some(&self.net.link) {
            if let Link::Down(e) = &self.net.link {
                eprintln!("sdroxide: solar feed link down: {e}");
            }
            self.reported = Some(self.net.link.clone());
        }

        // The fade bookkeeping is the panel map's, run here against the relayed
        // decodes so the browser's globe shows the same stations the main tab
        // does. `preview` has no equivalent: a clicked-but-unanswered decode is
        // state of the main window, which this tab is not.
        let now_t = ui.input(|i| i.time);
        self.stations.observe(&self.net.decodes, now_t, crate::time::now_unix());
        self.state.set_qth(&self.net.my_grid.clone());
        self.state.digi =
            self.stations.traffic(now_t, self.net.dx_grid.as_deref(), None, self.net.transmitting);

        overlay::ui(ui, &mut self.state);

        // Drain what the overlay asked for. `close_requested` is never set: the
        // browser owns the tab, and there is no window for us to stop emitting.
        if std::mem::take(&mut self.state.refresh_requested) {
            self.net.send(&SolarClientMsg::RefreshAll);
        }
        self.net.sync_channel(self.state.view.channel, self.state.view.resolution);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, STORAGE_KEY, &self.state.view);
    }
}
