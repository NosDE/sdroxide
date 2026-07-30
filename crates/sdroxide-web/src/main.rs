//! Browser client: the same `SdroxideApp` over a WebSocket
//! `RemoteController`, with audio through a JS AudioWorklet bridge.

#[cfg(target_arch = "wasm32")]
mod web {
    use eframe::wasm_bindgen::{self, JsCast, prelude::*};
    use sdroxide_proto::AudioCaps;
    use sdroxide_ui::{AudioBridge, RemoteController, SdroxideApp, SolarApp};

    // Implemented in assets/audio_bridge.js (loaded by index.html).
    #[wasm_bindgen(js_namespace = ["window", "sdroxideAudio"])]
    extern "C" {
        #[wasm_bindgen(js_name = pushPcm)]
        fn push_pcm(pcm: &[f32]);
        #[wasm_bindgen(js_name = pullMic)]
        fn pull_mic() -> Vec<f32>;
    }

    struct WebAudioBridge;

    impl AudioBridge for WebAudioBridge {
        fn caps(&self) -> AudioCaps {
            // PCM16 both ways for now; a WebCodecs Opus path can upgrade
            // this without protocol changes.
            AudioCaps { opus_decode: false, opus_encode: false }
        }
        fn play(&mut self, pcm: &[f32]) {
            push_pcm(pcm);
        }
        fn pull_mic(&mut self, out: &mut Vec<f32>) {
            out.extend(pull_mic());
        }
    }

    /// Which wgpu backend to ask the browser for.
    ///
    /// The default is wgpu's own: WebGPU where the browser has it, WebGL2
    /// otherwise. `?gfx=webgl` pins it to WebGL2 and `?gfx=webgpu` to WebGPU.
    ///
    /// The escape hatch exists because the solar view is by far this app's
    /// heaviest graphics consumer — depth, MSAA, a few dozen draws a frame —
    /// and browser WebGPU implementations are not uniformly ready for that.
    /// Firefox on Linux in particular has been seen to abort the *whole browser
    /// process* with this page open, which is a fault no amount of care on this
    /// side can prevent. WebGL2 draws the same scene; it loses MSAA and some
    /// depth precision, and that is the whole difference.
    fn web_options(search: &str) -> eframe::WebOptions {
        use sdroxide_ui::egui_wgpu::{self, wgpu};

        let backends = if search.contains("gfx=webgl") {
            wgpu::Backends::GL
        } else if search.contains("gfx=webgpu") {
            wgpu::Backends::BROWSER_WEBGPU
        } else {
            return eframe::WebOptions::default();
        };

        let mut setup = egui_wgpu::WgpuSetupCreateNew::without_display_handle();
        setup.instance_descriptor.backends = backends;
        eframe::WebOptions {
            wgpu_options: egui_wgpu::WgpuConfiguration {
                wgpu_setup: egui_wgpu::WgpuSetup::CreateNew(setup),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    pub fn run() {
        console_error_panic_hook::set_once();

        wasm_bindgen_futures::spawn_local(async {
            let window = web_sys::window().expect("window");
            let document = window.document().expect("document");
            let canvas = document
                .get_element_by_id("sdroxide_canvas")
                .expect("canvas element")
                .dyn_into::<web_sys::HtmlCanvasElement>()
                .expect("canvas type");

            // One wasm binary serves both pages, picked by the query string.
            // A second Trunk target would duplicate egui, eframe and wgpu — by
            // far the largest part of the bundle — in a second download, to
            // separate two views that share nearly all of their code.
            //
            // `?view=solar` also needs no server route of its own: it resolves
            // to the same index.html through the existing static fallback,
            // which is what lets the ☀ 3D chip open a plain relative URL.
            let location = window.location();
            let search = location.search().unwrap_or_default();
            let solar = search.contains("view=solar");
            let options = web_options(&search);

            let ws_proto =
                if location.protocol().as_deref() == Ok("https:") { "wss" } else { "ws" };
            let host = location.host().unwrap_or_else(|_| "localhost:4950".into());

            let runner = eframe::WebRunner::new();
            let result = if solar {
                // The map is a viewer: no audio bridge, so this tab never asks
                // for the microphone, and its own endpoint, so it does not take
                // the control slot the main tab holds.
                document.set_title("sdroxide — solar system");
                let url = format!("{ws_proto}://{host}/solar-ws");
                runner
                    .start(
                        canvas,
                        options,
                        Box::new(move |cc| {
                            Ok(Box::new(
                                SolarApp::new(cc, &url)
                                    .map_err(|e| format!("solar websocket connect: {e}"))?,
                            ))
                        }),
                    )
                    .await
            } else {
                let url = format!("{ws_proto}://{host}/ws");
                runner
                    .start(
                        canvas,
                        options,
                        // Connect inside the creator so the socket can wake the UI
                        // (repaint) the moment a message arrives.
                        Box::new(move |cc| {
                            let ctx = cc.egui_ctx.clone();
                            // Deadline hint, not an immediate repaint — see the
                            // native remote client for rationale.
                            let ctrl = RemoteController::connect(
                                &url,
                                Some(Box::new(WebAudioBridge)),
                                move || {
                                    ctx.request_repaint_after(std::time::Duration::from_millis(33))
                                },
                            )
                            .map_err(|e| format!("websocket connect: {e}"))?;
                            Ok(Box::new(SdroxideApp::new(cc, Box::new(ctrl))))
                        }),
                    )
                    .await
            };
            result.expect("eframe start");
        });
    }
}

fn main() {
    #[cfg(target_arch = "wasm32")]
    web::run();
}
