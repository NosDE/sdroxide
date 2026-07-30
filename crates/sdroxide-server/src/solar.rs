//! `/solar-ws`: the read-only feed behind the browser's solar-system view.
//!
//! Three things make this a separate route rather than more traffic on `/ws`:
//!
//! * **It is a viewer, not a controller.** Nothing here reaches `cmd_tx`; the
//!   only inbound messages pick an SDO channel and ask for a refresh. So it does
//!   not take the single-client slot `/ws` guards with `Shared::busy`, and any
//!   number of maps can be open without touching who owns PTT.
//! * **The feed is expensive and optional.** [`SolarFeed`] talks to thirteen
//!   outbound endpoints. It starts when the first viewer connects and is dropped
//!   when the last one leaves — the same contract the native window has, and the
//!   one the manual promises: nobody watching means no outbound request at all.
//! * **It is separately versioned.** See [`sdroxide_proto::solar`].
//!
//! The feed is a synchronous, thread-owning thing and this is a tokio server, so
//! one bridge thread (`sdroxide-solarpump`) owns the snapshot handle, diffs it,
//! and publishes into a `broadcast` that each viewer's socket task forwards.
//! That is the same shape as [`crate::pump`], for the same reason.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use sdroxide_proto::solar::{SOLAR_PROTO_VERSION, SolarClientMsg, SolarServerMsg};
use sdroxide_proto::{decode, encode};
use sdroxide_solar::{FeedCmd, RawUpdate, SdoChannel, SolarData, SolarFeed};

use crate::Shared;

/// How often the bridge thread re-reads the snapshot. The feed's own wake is
/// what usually drives a publish; this is the floor, and it is generous because
/// nothing here changes faster than every few minutes.
const POLL: Duration = Duration::from_millis(250);

/// Enough for the snapshot burst plus a comfortable backlog. A viewer that
/// falls this far behind is not going to catch up, and lagging it is better
/// than stalling the bridge for everyone else.
const BROADCAST_CAP: usize = 64;

/// What every viewer needs on connect, kept because a `broadcast` has no
/// history and the feed only republishes when something changes.
#[derive(Default)]
pub(crate) struct SolarLatest {
    pub sun: Option<SolarServerMsg>,
    pub aurora: Option<SolarServerMsg>,
    /// The two cloud channels, kept apart so a refreshed infrared frame does
    /// not drop the visible one a viewer has not seen yet.
    pub clouds_ir: Option<SolarServerMsg>,
    pub clouds_vis: Option<SolarServerMsg>,
    pub tles_amateur: Option<SolarServerMsg>,
    pub tles_geo: Option<SolarServerMsg>,
    pub weather: Option<SolarServerMsg>,
    pub events: Option<SolarServerMsg>,
    pub status: Option<SolarServerMsg>,
    /// Relayed from the radio's own event stream, not from the solar feed.
    pub digi: Option<SolarServerMsg>,
    pub decodes: Option<SolarServerMsg>,
}

impl SolarLatest {
    /// The connect burst, cheapest first: the panels and the globe come up
    /// before the ~150 kB solar disk arrives.
    fn snapshot(&self) -> Vec<SolarServerMsg> {
        [
            &self.status,
            &self.weather,
            &self.events,
            &self.digi,
            &self.decodes,
            &self.tles_amateur,
            &self.tles_geo,
            &self.aurora,
            &self.clouds_ir,
            &self.clouds_vis,
            &self.sun,
        ]
        .into_iter()
        .flatten()
        .cloned()
        .collect()
    }

    /// Forget everything the solar feed produced, leaving what the radio
    /// produced alone.
    ///
    /// The two have different lifecycles. Feed products die with the feed:
    /// serving them to a viewer half an hour later would present the readings
    /// of a stopped feed as current. `digi` and `decodes` come from the radio,
    /// which is still running — and the operator config is announced exactly
    /// once at engine start, so dropping it means no later viewer ever learns
    /// the QTH and the globe loses its home marker and its QSO arcs.
    fn clear_feed_products(&mut self) {
        self.sun = None;
        self.aurora = None;
        self.clouds_ir = None;
        self.clouds_vis = None;
        self.tles_amateur = None;
        self.tles_geo = None;
        self.weather = None;
        self.events = None;
        self.status = None;
    }

    fn record(&mut self, msg: &SolarServerMsg) {
        let slot = match msg {
            SolarServerMsg::Sun { .. } => &mut self.sun,
            SolarServerMsg::Aurora(_) => &mut self.aurora,
            SolarServerMsg::Clouds { infrared: true, .. } => &mut self.clouds_ir,
            SolarServerMsg::Clouds { infrared: false, .. } => &mut self.clouds_vis,
            SolarServerMsg::Tles { geo: false, .. } => &mut self.tles_amateur,
            SolarServerMsg::Tles { geo: true, .. } => &mut self.tles_geo,
            SolarServerMsg::Weather { .. } => &mut self.weather,
            SolarServerMsg::Events { .. } => &mut self.events,
            SolarServerMsg::Status(_) => &mut self.status,
            SolarServerMsg::Digi { .. } => &mut self.digi,
            SolarServerMsg::Decodes(_) => &mut self.decodes,
            // Per-connection handshake traffic is not part of a snapshot.
            SolarServerMsg::HelloAck { .. } | SolarServerMsg::Error(_) | SolarServerMsg::Pong => {
                return;
            }
        };
        *slot = Some(msg.clone());
    }
}

/// The viewer registry and the feed it keeps alive.
pub(crate) struct SolarHub {
    tx: broadcast::Sender<Arc<SolarServerMsg>>,
    pub latest: Mutex<SolarLatest>,
    /// Guards the feed handle and the viewer count together: they change as one
    /// (first viewer starts, last viewer stops) and must never disagree.
    feed: Mutex<FeedState>,
}

struct FeedState {
    viewers: usize,
    feed: Option<SolarFeed>,
    /// Tells the bridge thread to exit. A generation counter rather than a
    /// flag, so a feed restarted before the old thread noticed cannot make the
    /// new thread exit.
    generation: u64,
    channel: SdoChannel,
    resolution: u32,
}

impl Default for SolarHub {
    fn default() -> Self {
        SolarHub {
            tx: broadcast::channel(BROADCAST_CAP).0,
            latest: Mutex::new(SolarLatest::default()),
            feed: Mutex::new(FeedState {
                viewers: 0,
                feed: None,
                generation: 0,
                // Matches `Solar3dView::default()`: HMI continuum at 1024², so
                // the first thing a viewer sees is white light with real spots.
                channel: SdoChannel::from_u8(0),
                resolution: 1024,
            }),
        }
    }
}

impl SolarHub {
    /// Publish to every viewer and remember it for the next one's snapshot.
    pub(crate) fn publish(&self, msg: SolarServerMsg) {
        self.latest.lock().unwrap().record(&msg);
        // An error here only means nobody is watching.
        let _ = self.tx.send(Arc::new(msg));
    }

    fn send_cmd(&self, cmd: FeedCmd) {
        if let Some(f) = self.feed.lock().unwrap().feed.as_ref() {
            f.send(cmd);
        }
    }
}

pub async fn ws_route(State(shared): State<Arc<Shared>>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(|socket| session(socket, shared))
}

fn msg(m: &SolarServerMsg) -> Message {
    Message::Binary(encode(m).expect("encode").into())
}

async fn session(mut socket: WebSocket, shared: Arc<Shared>) {
    // Subscribe *before* the snapshot, so an update that lands between the two
    // is queued rather than missed.
    let mut rx = shared.solar.tx.subscribe();

    // --- Hello handshake (5 s budget) ---------------------------------
    let hello = tokio::time::timeout(Duration::from_secs(5), socket.recv()).await;
    match hello {
        Ok(Some(Ok(Message::Binary(bytes)))) => match decode::<SolarClientMsg>(&bytes) {
            Ok(SolarClientMsg::Hello { proto }) if proto == SOLAR_PROTO_VERSION => {}
            Ok(SolarClientMsg::Hello { proto }) => {
                let _ = socket
                    .send(msg(&SolarServerMsg::Error(format!(
                        "solar protocol mismatch: server {SOLAR_PROTO_VERSION}, client {proto}"
                    ))))
                    .await;
                return;
            }
            _ => {
                let _ = socket.send(msg(&SolarServerMsg::Error("expected Hello".into()))).await;
                return;
            }
        },
        _ => return,
    }
    if socket.send(msg(&SolarServerMsg::HelloAck { proto: SOLAR_PROTO_VERSION })).await.is_err() {
        return;
    }

    let viewers = acquire_feed(&shared);
    info!(viewers, "solar viewer connected");

    // Collect and release before sending: the guard must not be held across an
    // await, or the whole session future stops being `Send` and every publisher
    // blocks behind one slow socket.
    let snapshot = shared.solar.latest.lock().unwrap().snapshot();
    for m in snapshot {
        if socket.send(msg(&m)).await.is_err() {
            release_feed(&shared);
            return;
        }
    }

    let (mut ws_tx, mut ws_rx) = futures_util::StreamExt::split(socket);

    let sender = async {
        loop {
            match rx.recv().await {
                Ok(m) => {
                    if ws_tx.send(msg(&m)).await.is_err() {
                        break;
                    }
                }
                // A viewer that fell behind has missed updates but is still
                // connected; the next publish of each product brings it back
                // into step, so keep going rather than dropping it.
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("solar viewer lagged {n} messages");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    let receiver = async {
        while let Some(Ok(m)) = ws_rx.next().await {
            let Message::Binary(bytes) = m else { continue };
            match decode::<SolarClientMsg>(&bytes) {
                // The feed is shared, so this changes the picture for every
                // viewer. Running one feed per viewer would multiply the
                // outbound traffic to NASA by the number of open tabs.
                Ok(SolarClientMsg::SetChannel(c)) => {
                    let ch = SdoChannel::from_u8(c);
                    shared.solar.feed.lock().unwrap().channel = ch;
                    shared.solar.send_cmd(FeedCmd::SetChannel(ch));
                }
                Ok(SolarClientMsg::SetResolution(r)) => {
                    let res = u32::from(r);
                    shared.solar.feed.lock().unwrap().resolution = res;
                    shared.solar.send_cmd(FeedCmd::SetResolution(res));
                }
                Ok(SolarClientMsg::RefreshAll) => shared.solar.send_cmd(FeedCmd::RefreshAll),
                Ok(SolarClientMsg::Ping) => shared.solar.publish(SolarServerMsg::Pong),
                Ok(SolarClientMsg::Hello { .. }) => {}
                Err(e) => debug!("solar viewer sent an undecodable message: {e}"),
            }
        }
    };

    tokio::select! {
        _ = sender => {}
        _ = receiver => {}
    }

    let left = release_feed(&shared);
    info!(viewers = left, "solar viewer disconnected");
}

/// Register a viewer, starting the feed if this is the first. Returns the new
/// viewer count.
fn acquire_feed(shared: &Arc<Shared>) -> usize {
    let mut st = shared.solar.feed.lock().unwrap();
    st.viewers += 1;
    if st.feed.is_some() {
        return st.viewers;
    }

    st.generation += 1;
    let generation = st.generation;
    let (raw_tx, raw_rx) = crossbeam_channel::unbounded();
    let (wake_tx, wake_rx) = crossbeam_channel::bounded(1);
    let feed = SolarFeed::start_with_raw(
        st.channel,
        st.resolution,
        // Latest-wins: a full slot already means "there is work to do".
        move || {
            let _ = wake_tx.try_send(());
        },
        Some(raw_tx),
    );
    let data = feed.shared();
    st.feed = Some(feed);
    let viewers = st.viewers;
    drop(st);

    let shared = Arc::clone(shared);
    std::thread::Builder::new()
        .name("sdroxide-solarpump".into())
        .spawn(move || solar_pump(shared, data, raw_rx, wake_rx, generation))
        .expect("spawn solar pump thread");
    info!("solar feed started");
    viewers
}

/// Deregister a viewer, dropping the feed when the last one goes. Returns the
/// remaining viewer count.
fn release_feed(shared: &Arc<Shared>) -> usize {
    let mut st = shared.solar.feed.lock().unwrap();
    st.viewers = st.viewers.saturating_sub(1);
    if st.viewers == 0 {
        // Dropping the handle disconnects the worker's command channel, which
        // is how it learns to stop. This is what confines every outbound
        // request to the lifetime of an open map.
        st.feed = None;
        st.generation += 1;
        // The feed's cached products go with it: serving a viewer ten minutes
        // from now with readings from a feed that has since stopped would
        // present stale numbers as current. What the radio published stays.
        shared.solar.latest.lock().unwrap().clear_feed_products();
        info!("solar feed stopped: no viewers left");
    }
    st.viewers
}

/// Bridge thread: the feed's snapshot and raw payloads → broadcast messages.
fn solar_pump(
    shared: Arc<Shared>,
    data: Arc<Mutex<SolarData>>,
    raw_rx: crossbeam_channel::Receiver<RawUpdate>,
    wake_rx: crossbeam_channel::Receiver<()>,
    generation: u64,
) {
    let mut sent = SentMarks::default();

    loop {
        // Either the feed woke us or the poll floor expired; both mean "look".
        let _ = wake_rx.recv_timeout(POLL);

        if shared.solar.feed.lock().unwrap().generation != generation {
            break;
        }

        // Raw payloads first: a viewer should not see the status say the image
        // is current before the image itself has been published.
        while let Ok(u) = raw_rx.try_recv() {
            match u {
                RawUpdate::Sun { channel, fetched_unix, jpeg } => {
                    shared.solar.publish(SolarServerMsg::Sun {
                        channel: channel.to_u8(),
                        fetched_unix,
                        jpeg,
                    });
                }
                RawUpdate::Tle { geo, text } => {
                    shared.solar.publish(SolarServerMsg::Tles { geo, text });
                }
                RawUpdate::Clouds { band, frame_unix, fetched_unix, png } => {
                    shared.solar.publish(SolarServerMsg::Clouds {
                        infrared: band == sdroxide_solar::Band::Longwave,
                        frame_unix,
                        fetched_unix,
                        png,
                    });
                }
            }
        }

        // Everything else is a diff against what this thread last published.
        let (weather, events, status, aurora) = {
            let d = data.lock().unwrap_or_else(|e| e.into_inner());
            let weather = (sent.weather.as_ref() != Some(&d.weather)
                || sent.aurora_power.as_ref() != Some(&d.aurora_power)
                || sent.kp_forecast.as_ref() != Some(&d.kp_forecast))
            .then(|| {
                sent.weather = Some(d.weather.clone());
                sent.aurora_power = Some(d.aurora_power);
                sent.kp_forecast = Some(d.kp_forecast.clone());
                SolarServerMsg::Weather {
                    weather: d.weather.clone(),
                    aurora_power: d.aurora_power,
                    kp_forecast: d.kp_forecast.clone(),
                }
            });

            let events = (sent.cmes.as_ref() != Some(&d.cmes)
                || sent.flares.as_ref() != Some(&d.flares)
                || sent.regions.as_ref() != Some(&d.regions))
            .then(|| {
                sent.cmes = Some(d.cmes.clone());
                sent.flares = Some(d.flares.clone());
                sent.regions = Some(d.regions.clone());
                SolarServerMsg::Events {
                    cmes: d.cmes.clone(),
                    flares: d.flares.clone(),
                    regions: d.regions.clone(),
                }
            });

            let status = (sent.status.as_deref() != Some(&d.status[..])).then(|| {
                sent.status = Some(d.status.to_vec());
                SolarServerMsg::Status(d.status.to_vec())
            });

            // The oval is the one product whose generation counter is the
            // cheapest thing to compare — the grid itself is 65 kB.
            let aurora = (d.aurora_gen != sent.aurora_gen)
                .then(|| {
                    sent.aurora_gen = d.aurora_gen;
                    d.aurora.as_deref().cloned().map(SolarServerMsg::Aurora)
                })
                .flatten();

            (weather, events, status, aurora)
        };

        for m in [weather, events, aurora, status].into_iter().flatten() {
            shared.solar.publish(m);
        }
    }
    debug!("solar pump thread stopped");
}

/// What the bridge thread has already published, so it sends diffs rather than
/// the whole snapshot every quarter second.
#[derive(Default)]
struct SentMarks {
    weather: Option<sdroxide_solar::SpaceWeather>,
    aurora_power: Option<Option<sdroxide_solar::HemisphericPower>>,
    kp_forecast: Option<Vec<sdroxide_solar::KpPoint>>,
    cmes: Option<Vec<sdroxide_solar::CmeEvent>>,
    flares: Option<Vec<sdroxide_solar::FlareEvent>>,
    regions: Option<Vec<sdroxide_solar::ActiveRegion>>,
    status: Option<Vec<sdroxide_solar::SourceStatus>>,
    aurora_gen: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdroxide_solar::{AuroraOval, Source};

    /// The connect burst has to put the cheap products first: a viewer that
    /// waits on a megabyte of solar disk before it can draw the globe looks
    /// broken for the whole download.
    #[test]
    fn the_snapshot_sends_the_image_last() {
        let mut l = SolarLatest::default();
        for m in [
            SolarServerMsg::Sun { channel: 0, fetched_unix: 1, jpeg: vec![1] },
            SolarServerMsg::Status(vec![Default::default(); Source::ALL.len()]),
            SolarServerMsg::Weather {
                weather: Default::default(),
                aurora_power: None,
                kp_forecast: Vec::new(),
            },
        ] {
            l.record(&m);
        }
        let snap = l.snapshot();
        assert_eq!(snap.len(), 3);
        assert!(matches!(snap[0], SolarServerMsg::Status(_)));
        assert!(matches!(snap.last(), Some(SolarServerMsg::Sun { .. })));
    }

    /// Handshake traffic is per-connection. Caching a `HelloAck` would replay
    /// somebody else's handshake into the next viewer's snapshot.
    #[test]
    fn handshake_messages_never_enter_the_snapshot() {
        let mut l = SolarLatest::default();
        l.record(&SolarServerMsg::HelloAck { proto: SOLAR_PROTO_VERSION });
        l.record(&SolarServerMsg::Pong);
        l.record(&SolarServerMsg::Error("boom".into()));
        assert!(l.snapshot().is_empty());
    }

    /// Stopping the feed must not discard what the radio said. The operator
    /// config is announced once, at engine start; if it is dropped when the
    /// last viewer leaves, every later viewer comes up with no QTH — no home
    /// marker, no QSO arcs — and nothing will ever re-send it.
    #[test]
    fn stopping_the_feed_keeps_the_radio_side_of_the_snapshot() {
        let mut l = SolarLatest::default();
        l.record(&SolarServerMsg::Digi {
            my_grid: "JN78ve".into(),
            dx_grid: None,
            transmitting: false,
        });
        l.record(&SolarServerMsg::Decodes(Vec::new()));
        l.record(&SolarServerMsg::Sun { channel: 0, fetched_unix: 1, jpeg: vec![1] });
        l.record(&SolarServerMsg::Aurora(AuroraOval {
            observed_unix: 1,
            forecast_unix: 2,
            grid: vec![0],
        }));

        l.clear_feed_products();
        let snap = l.snapshot();
        assert_eq!(snap.len(), 2, "expected only the radio's messages, got {snap:?}");
        assert!(snap.iter().any(|m| matches!(m, SolarServerMsg::Digi { .. })));
        assert!(snap.iter().any(|m| matches!(m, SolarServerMsg::Decodes(_))));
    }

    /// Both element sets have to survive independently, or QO-100 overwrites
    /// the amateur list and the map loses every LEO satellite.
    #[test]
    fn the_two_element_sets_have_their_own_slots() {
        let mut l = SolarLatest::default();
        l.record(&SolarServerMsg::Tles { geo: false, text: "amateur".into() });
        l.record(&SolarServerMsg::Tles { geo: true, text: "qo100".into() });
        let snap = l.snapshot();
        assert_eq!(snap.len(), 2);
        assert!(snap.contains(&SolarServerMsg::Tles { geo: false, text: "amateur".into() }));
        assert!(snap.contains(&SolarServerMsg::Tles { geo: true, text: "qo100".into() }));
    }
}
