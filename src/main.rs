mod audio_cat_source;
mod console;
mod flex_source;
mod gui_main;
mod hpsdr_source;
mod icom_source;
mod local_controller;
mod null_source;
mod rtlsdr_source;
mod server_main;
mod tci_source;

use anyhow::{Context, bail};
use clap::Parser;
use sdroxide_config::Settings;
use sdroxide_radio::{FileSource, IqSource, SigGenSource};
#[cfg(feature = "soapy")]
use sdroxide_radio::{SoapyDevice, enumerate_devices};
use sdroxide_types::{Backend, DeviceCaps, RadioConfig};

#[derive(Parser, Debug, Clone)]
#[command(version, about)]
struct Cli {
    /// SoapySDR device args, e.g. "driver=hackrf" (default: config, then first device)
    #[arg(long)]
    device: Option<String>,

    /// List devices and their probed capabilities, then exit
    #[arg(long)]
    probe: bool,

    /// Terminal waterfall mode
    #[arg(long)]
    console: bool,

    /// Use the built-in signal generator instead of hardware
    #[arg(long)]
    siggen: bool,

    /// Play a raw interleaved CF32 IQ file instead of hardware
    #[arg(long)]
    file: Option<std::path::PathBuf>,

    /// Center frequency in Hz
    #[arg(long, default_value_t = 14_200_000.0)]
    freq: f64,

    /// Sample rate in Hz (default: from config)
    #[arg(long)]
    rate: Option<f64>,

    /// Overall RX gain in dB (default: hardware AGC or a moderate value)
    #[arg(long)]
    gain: Option<f64>,

    /// Initial mode (USB, LSB, CW, AM, SAM, NFM, WFM, DIGU, DIGL, DSB, SPEC, FT8,
    /// FT4, PSK, RTTY, SSTV, RIFP, OLIVIA, THOR, FSQ)
    #[arg(long)]
    mode: Option<sdroxide_types::Mode>,

    /// Headless TX smoke test: key a tune carrier for SECS seconds at the
    /// configured (minimal) drive and gains, then exit
    #[arg(long, value_name = "SECS")]
    tx_tune: Option<f64>,

    /// Headless FT8 smoke test: call CQ (with a test callsign) at minimal
    /// power for ~SECS seconds, report whether a slot-aligned burst keyed
    #[arg(long, value_name = "SECS")]
    ft8_cq: Option<f64>,

    /// Headless RADE smoke test: run the digital-voice receiver for ~SECS
    /// seconds (pair with --file) and report whether the modem reached sync
    #[arg(long, value_name = "SECS")]
    rade_rx: Option<f64>,

    /// Connect to FreeDV Reporter read-only for ~SECS seconds and print what
    /// arrives. Uses the server's "view" role: nothing is reported and this
    /// station does not appear on qso.freedv.org. Needs no radio.
    #[arg(long, value_name = "SECS")]
    freedv_reporter_probe: Option<f64>,

    /// FreeDV Reporter host for --freedv-reporter-probe
    #[arg(long, value_name = "HOST[:PORT]", default_value = "qso.freedv.org")]
    freedv_reporter_host: String,

    /// Run as a server: HTTP web client + WebSocket streaming backend
    #[arg(long)]
    server: bool,

    /// Connect as a native remote client to a running sdroxide server
    /// (e.g. "host:4950" or a full ws:// URL)
    #[arg(long, value_name = "HOST[:PORT]")]
    connect: Option<String>,

    /// Server port (default: from config)
    #[arg(long)]
    port: Option<u16>,

    /// Directory with the trunk-built web client (default: embedded assets
    /// if compiled with --features embed-web)
    #[arg(long)]
    web_root: Option<std::path::PathBuf>,

    /// Spectrum FFT size
    #[arg(long, default_value_t = 4096)]
    fft: usize,

    /// Console waterfall lines per second
    #[arg(long, default_value_t = 15)]
    fps: u32,

    /// Display floor in dBFS
    #[arg(long, default_value_t = -110.0, allow_negative_numbers = true)]
    db_floor: f32,

    /// Display ceiling in dBFS
    #[arg(long, default_value_t = -10.0, allow_negative_numbers = true)]
    db_ceil: f32,

    /// Console spectrum width in characters
    #[arg(long, default_value_t = 100)]
    width: usize,

    /// Allow transmit on any frequency the hardware supports, not just the
    /// amateur bands
    ///
    /// Overrides `tx_ham_only` in config.toml for this run only, and puts a
    /// warning on screen that has to be dismissed by hand. For licensed
    /// out-of-band use — MARS/CAP, a commercial or experimental licence, a
    /// dummy load — where transmitting outside the amateur allocations is
    /// something you are authorised to do. Everywhere else it is an offence,
    /// and the band edges are the last thing standing between a mistyped
    /// frequency and an interference complaint.
    #[arg(long)]
    oob_tx: bool,
}

impl Cli {
    /// Whether the engine should refuse to key outside the amateur bands.
    ///
    /// The flag can only ever *loosen* the config, never tighten it: a build
    /// without it behaves exactly as before.
    fn tx_ham_only(&self, settings: &Settings) -> bool {
        settings.tx_ham_only && !self.oob_tx
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    let settings = Settings::load();

    if cli.probe {
        return probe(&cli, &settings);
    }
    if cli.console {
        let (source, _caps) = open_source(&cli, &settings)?;
        return console::run(
            source,
            console::Options {
                fft_size: cli.fft,
                fps: cli.fps.max(1),
                db_floor: cli.db_floor,
                db_ceil: cli.db_ceil,
                width: cli.width.clamp(16, 400),
            },
        );
    }

    if let Some(secs) = cli.tx_tune {
        let (source, caps) = open_source(&cli, &settings)?;
        return tx_tune_test(source, caps, cli.tx_ham_only(&settings), secs.clamp(0.2, 10.0));
    }
    if let Some(secs) = cli.ft8_cq {
        let (source, caps) = open_source(&cli, &settings)?;
        return ft8_cq_test(source, caps, cli.tx_ham_only(&settings), secs.clamp(16.0, 60.0));
    }
    if let Some(secs) = cli.rade_rx {
        let (source, caps) = open_source(&cli, &settings)?;
        return rade_rx_test(source, caps, cli.tx_ham_only(&settings), secs.clamp(2.0, 120.0));
    }
    // Before any radio setup: the probe talks to the network and nothing else.
    if let Some(secs) = cli.freedv_reporter_probe {
        return freedv_reporter_probe(&cli.freedv_reporter_host, secs.clamp(2.0, 300.0));
    }
    if cli.server {
        let (source, caps) = open_source(&cli, &settings)?;
        let port = cli.port.unwrap_or(settings.server_port);
        return server_main::run(
            source,
            caps,
            &settings,
            cli.tx_ham_only(&settings),
            cli.mode,
            port,
            cli.web_root.clone(),
            Some(reopen_factory(&cli)),
        );
    }
    if let Some(target) = &cli.connect {
        let url = if target.contains("://") { target.clone() } else { format!("ws://{target}/ws") };
        return gui_main::run_remote(&url);
    }

    let (source, caps) = open_source(&cli, &settings)?;
    gui_main::run(
        source,
        caps,
        &settings,
        cli.tx_ham_only(&settings),
        cli.mode,
        Some(reopen_factory(&cli)),
    )
}

/// Factory the engine calls to rebuild the interface at runtime: when the
/// operator switches interface (Settings → Radio → Apply), so that never needs a
/// restart, and when the engine reconnects an interface that wasn't there yet.
/// Re-reads the persisted radio config + settings each call and opens at the
/// current dial. Fallible so a bad new config leaves the current interface
/// running.
fn reopen_factory(cli: &Cli) -> sdroxide_radio::ReopenFn {
    let cli = cli.clone();
    Box::new(move |center: f64| {
        let mut c = cli.clone();
        c.freq = center;
        let settings = Settings::load();
        if c.siggen || c.file.is_some() {
            return open_source(&c, &settings).map_err(|e| format!("{e:#}"));
        }
        let radio = sdroxide_config::load_radio_config();
        open_configured_source(&radio, &c, &settings).map_err(|e| format!("{e:#}"))
    })
}

/// Headless tune-carrier smoke test. Relies on the engine safety rails:
/// TX hardware gains at minimum, tune drive default 5%, ham-band lockout.
fn tx_tune_test(
    source: Box<dyn IqSource>,
    caps: sdroxide_types::DeviceCaps,
    tx_ham_only: bool,
    secs: f64,
) -> anyhow::Result<()> {
    use sdroxide_types::{Command, RadioEvent};
    use std::time::Duration;

    let mut handles = sdroxide_radio::start_engine(
        source,
        caps,
        sdroxide_radio::EngineConfig { tx_ham_only, ..Default::default() },
    );
    let engine_thread = handles.thread.take();
    std::thread::sleep(Duration::from_millis(400));
    handles.cmd_tx.send(Command::SetTune(true))?;
    std::thread::sleep(Duration::from_secs_f64(secs));
    handles.cmd_tx.send(Command::SetTune(false))?;
    std::thread::sleep(Duration::from_millis(400));

    let mut keyed = false;
    let mut failure = None;
    while let Ok(ev) = handles.event_rx.try_recv() {
        match ev {
            RadioEvent::State(s) => keyed |= s.tx.tune,
            RadioEvent::ConnectionLost(e) => failure = Some(e),
            _ => {}
        }
    }
    let outcome = match (keyed, failure) {
        (_, Some(e)) => Err(anyhow::anyhow!("TX test failed: {e}")),
        (false, None) => {
            Err(anyhow::anyhow!("TX was refused (safety rails or device limits) — see log"))
        }
        (true, None) => {
            println!("TX tune test OK: carrier keyed for {secs:.1} s and released.");
            Ok(())
        }
    };
    drop(handles);
    if let Some(t) = engine_thread {
        let _ = t.join();
    }
    outcome
}

/// Headless FT8 smoke test: configure a test callsign, enter FT8, call CQ,
/// and confirm the engine keys a slot-aligned burst. Minimal drive / min TX
/// gain (same emission level as `--tx-tune`).
fn ft8_cq_test(
    source: Box<dyn IqSource>,
    caps: sdroxide_types::DeviceCaps,
    tx_ham_only: bool,
    secs: f64,
) -> anyhow::Result<()> {
    use sdroxide_types::{Command, DigiConfig, Mode, RadioEvent, RxId};
    use std::time::Duration;

    let mut handles = sdroxide_radio::start_engine(
        source,
        caps,
        sdroxide_radio::EngineConfig {
            tx_ham_only,
            initial_mode: Some(Mode::Ft8),
            ..Default::default()
        },
    );
    let engine_thread = handles.thread.take();
    std::thread::sleep(Duration::from_millis(400));

    let cfg = DigiConfig { my_call: "AB1CD".into(), my_grid: "FN42".into(), ..Default::default() };
    handles.cmd_tx.send(Command::SetMode { rx: RxId::Main, mode: Mode::Ft8 })?;
    handles.cmd_tx.send(Command::SetDigiConfig(cfg))?;
    handles.cmd_tx.send(Command::DigiCallCq)?;

    let mut keyed = false;
    let mut failure = None;
    let deadline = std::time::Instant::now() + Duration::from_secs_f64(secs);
    while std::time::Instant::now() < deadline {
        while let Ok(ev) = handles.event_rx.try_recv() {
            match ev {
                RadioEvent::State(s) => keyed |= s.tx.ptt,
                RadioEvent::Ft8Status(s) if s.transmitting => keyed = true,
                RadioEvent::ConnectionLost(e) => failure = Some(e),
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    handles.cmd_tx.send(Command::DigiStopQso)?;
    handles.cmd_tx.send(Command::DigiAbortTx)?;
    std::thread::sleep(Duration::from_millis(300));

    let outcome = match (keyed, failure) {
        (_, Some(e)) => Err(anyhow::anyhow!("FT8 CQ test failed: {e}")),
        (false, None) => Err(anyhow::anyhow!(
            "no FT8 burst keyed in {secs:.0}s — check UTC clock / safety rails (see log)"
        )),
        (true, None) => {
            println!("FT8 CQ test OK: a slot-aligned burst keyed and released.");
            Ok(())
        }
    };
    drop(handles);
    if let Some(t) = engine_thread {
        let _ = t.join();
    }
    outcome
}

/// Read-only FreeDV Reporter check: connect, listen, and print what the server
/// sends.
///
/// This uses the reporter's `"view"` role, which receives every broadcast but
/// never joins the public roster — so it is safe to point at qso.freedv.org.
/// Going *visible* requires an operator enabling the feature in Settings with a
/// real callsign; there is deliberately no flag for it here.
fn freedv_reporter_probe(host_arg: &str, secs: f64) -> anyhow::Result<()> {
    let (host, port) = match host_arg.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().unwrap_or(80)),
        None => (host_arg, 80),
    };
    println!("FreeDV Reporter probe (view role — not visible to others): {host}:{port}");
    println!("listening for {secs:.0} s…\n");

    let s = sdroxide_net::freedv_reporter_probe(host, port, secs);

    if s.event_counts.is_empty() {
        println!("events: (none)");
    } else {
        let counts: Vec<String> =
            s.event_counts.iter().map(|(name, n)| format!("{name} {n}")).collect();
        println!("events: {}", counts.join("  "));
    }
    let with_freq = s.stations.iter().filter(|st| st.freq_hz > 0).count();
    println!("stations: {} ({with_freq} with a frequency)\n", s.stations.len());
    for st in &s.stations {
        println!(
            "  {:<10} {:<8} {:>10.3} kHz  {:<8} {:<3} {}",
            st.call,
            st.grid,
            st.freq_hz as f64 / 1000.0,
            st.mode,
            if st.tx {
                "TX"
            } else if st.rx_only {
                "RX"
            } else {
                ""
            },
            st.version,
        );
    }
    println!(
        "\nhandshake: open={} connect_ack={} connection_successful={}",
        s.got_open, s.got_connect_ack, s.got_connection_successful
    );
    if let Some(status) = &s.last_status {
        println!("status: {status}");
    }
    if s.ok() {
        println!("\nPASS");
        Ok(())
    } else {
        bail!("FreeDV Reporter probe did not complete a session with at least one station")
    }
}

/// Headless RADE receive check: bring the engine up in RADE mode and report
/// what the modem made of whatever the source is feeding it.
///
/// Pair with `--file` and an IQ file from `rade-harness iq` for a repeatable
/// end-to-end run with no radio and no sound card.
fn rade_rx_test(
    source: Box<dyn IqSource>,
    caps: sdroxide_types::DeviceCaps,
    tx_ham_only: bool,
    secs: f64,
) -> anyhow::Result<()> {
    use sdroxide_types::{Command, Mode, RadioEvent, RxId};
    use std::time::Duration;

    // The receive chain — and with it the decoder's audio tap — only exists
    // when the engine has somewhere to play audio. Give it a ring buffer we
    // drain and throw away, so the test needs no sound card.
    let (producer, mut sink) = rtrb::RingBuffer::<f32>::new(96_000);
    let mut handles = sdroxide_radio::start_engine(
        source,
        caps,
        sdroxide_radio::EngineConfig {
            tx_ham_only,
            initial_mode: Some(Mode::Rade),
            audio: Some(sdroxide_radio::AudioParams { producer, out_rate: 48_000.0 }),
            ..Default::default()
        },
    );
    let engine_thread = handles.thread.take();
    handles.cmd_tx.send(Command::SetMode { rx: RxId::Main, mode: Mode::Rade })?;

    let (mut synced, mut best_snr, mut eoo, mut dropped) = (false, f32::MIN, 0u64, 0u64);
    let mut failure = None;
    let deadline = std::time::Instant::now() + Duration::from_secs_f64(secs);
    while std::time::Instant::now() < deadline {
        while let Ok(ev) = handles.event_rx.try_recv() {
            match ev {
                RadioEvent::Ft8Status(s) => {
                    if let Some(r) = s.rade {
                        if r.sync {
                            synced = true;
                            best_snr = best_snr.max(r.snr_db);
                        }
                        eoo = eoo.max(r.eoo_count);
                        dropped = dropped.max(r.dropped);
                    }
                }
                RadioEvent::ConnectionLost(e) => failure = Some(e),
                _ => {}
            }
        }
        while sink.pop().is_ok() {}
        std::thread::sleep(Duration::from_millis(50));
    }

    let outcome = match (synced, failure) {
        (_, Some(e)) => Err(anyhow::anyhow!("RADE test failed: {e}")),
        (false, None) => Err(anyhow::anyhow!(
            "RADE never reached sync in {secs:.0}s — is the source carrying a RADE signal?"
        )),
        (true, None) => {
            println!(
                "RADE RX test OK: sync reached, best SNR {best_snr:.1} dB, \
                 {eoo} end-of-over frame(s), {dropped} samples dropped."
            );
            Ok(())
        }
    };
    drop(handles);
    if let Some(t) = engine_thread {
        let _ = t.join();
    }
    outcome
}

#[cfg(feature = "soapy")]
fn device_filter(cli: &Cli, settings: &Settings) -> String {
    cli.device.clone().unwrap_or_else(|| settings.device_args.clone())
}

fn probe(cli: &Cli, settings: &Settings) -> anyhow::Result<()> {
    // RTL-SDR first, and in every build: the native driver needs no system
    // library, so this half of `--probe` works even in the non-SoapySDR
    // variant. It is the field-diagnosis tool for "does this machine see my
    // dongle, and may this user have it?".
    probe_rtlsdr();
    probe_soapy(cli, settings)
}

fn probe_rtlsdr() {
    let devices = sdroxide_rtlsdr::list();
    if devices.is_empty() {
        println!("No RTL-SDR dongles found on USB.");
    } else {
        println!("=== RTL-SDR (native USB driver) ===");
        for (i, d) in devices.iter().enumerate() {
            println!("  {}: {}  [usb {:04x}:{:04x}]", i, d.label(), d.vid, d.pid);
        }
    }
    println!();
}

#[cfg(feature = "soapy")]
fn probe_soapy(cli: &Cli, settings: &Settings) -> anyhow::Result<()> {
    let filter = device_filter(cli, settings);
    let devices = enumerate_devices(&filter).context("SoapySDR enumeration failed")?;
    if devices.is_empty() {
        println!("No SoapySDR devices found (filter: {:?}).", filter);
        return Ok(());
    }
    for (i, d) in devices.iter().enumerate() {
        println!("=== Device {}: {} [{}] ===", i, d.label, d.driver);
        match SoapyDevice::open(&d.args) {
            Ok(dev) => print_caps(dev.caps()),
            Err(e) => println!("  failed to open: {e}"),
        }
    }
    Ok(())
}

#[cfg(not(feature = "soapy"))]
fn probe_soapy(_cli: &Cli, _settings: &Settings) -> anyhow::Result<()> {
    println!("This build has no SoapySDR support (built with --no-default-features).");
    Ok(())
}

#[cfg(feature = "soapy")]
fn print_caps(caps: &sdroxide_types::DeviceCaps) {
    let fmt_mhz = |hz: f64| format!("{:.3} MHz", hz / 1e6);
    println!("  driver        : {}", caps.driver);
    println!("  label         : {}", caps.label);
    println!(
        "  channels      : {} RX, {} TX{}",
        caps.rx_channels,
        caps.tx_channels,
        if caps.tx_channels > 0 {
            if caps.full_duplex { " (full duplex)" } else { " (half duplex)" }
        } else {
            " (receive only)"
        }
    );
    for (name, ranges) in [("RX freq", &caps.freq_ranges_rx), ("TX freq", &caps.freq_ranges_tx)] {
        if !ranges.is_empty() {
            let list: Vec<String> = ranges
                .iter()
                .map(|&(lo, hi)| format!("{} – {}", fmt_mhz(lo), fmt_mhz(hi)))
                .collect();
            println!("  {:<13} : {}", name, list.join(", "));
        }
    }
    if !caps.sample_rates.is_empty() {
        let list: Vec<String> =
            caps.sample_rates.iter().map(|r| format!("{:.3}", r / 1e6)).collect();
        println!("  rates (Msps)  : {}", list.join(", "));
    }
    for &(lo, hi) in &caps.rate_ranges {
        println!("  rate range    : {:.3} – {:.3} Msps", lo / 1e6, hi / 1e6);
    }
    for g in &caps.gains {
        println!(
            "  gain {:<8} : {:?} {} to {} dB (step {})",
            g.name, g.direction, g.min_db, g.max_db, g.step_db
        );
    }
    if !caps.antennas_rx.is_empty() {
        println!("  RX antennas   : {}", caps.antennas_rx.join(", "));
    }
    if !caps.antennas_tx.is_empty() {
        println!("  TX antennas   : {}", caps.antennas_tx.join(", "));
    }
    if !caps.sensors.is_empty() {
        println!("  sensors       : {}", caps.sensors.join(", "));
    }
}

fn open_source(cli: &Cli, settings: &Settings) -> anyhow::Result<(Box<dyn IqSource>, DeviceCaps)> {
    let rate = cli.rate.unwrap_or(settings.sample_rate);

    if cli.siggen {
        return Ok((
            Box::new(SigGenSource::demo(rate, cli.freq)),
            synthetic_caps("Signal generator"),
        ));
    }
    if let Some(path) = &cli.file {
        let label = format!("IQ file {}", path.display());
        return Ok((
            Box::new(
                FileSource::open(path, rate, cli.freq)
                    .with_context(|| format!("opening IQ file {}", path.display()))?,
            ),
            synthetic_caps(&label),
        ));
    }

    // Try the configured radio interface. If it can't be opened (no SoapySDR
    // device, HPSDR unreachable, CAT port missing, TCI server not up yet, …)
    // fall back to a null source so the GUI — and the Settings dialog — still
    // come up, instead of the program refusing to launch. The engine keeps
    // retrying this same interface in the background (`IqSource::needs_reopen`),
    // so a rig that simply wasn't ready attaches by itself; Settings → Radio is
    // only needed to choose a *different* one.
    let radio = sdroxide_config::load_radio_config();
    match open_configured_source(&radio, cli, settings) {
        Ok(pair) => Ok(pair),
        Err(e) => {
            tracing::warn!("radio interface unavailable: {e:#}");
            let msg =
                format!("{e}. Retrying — or open Settings → Radio to choose another interface.");
            Ok((Box::new(null_source::NullSource::new(cli.freq, msg)), synthetic_caps("No radio")))
        }
    }
}

/// Open the interface selected in `radio.json`. `Auto` prefers a SoapySDR device
/// and falls back to CAT when none is present (or the binary has no soapy).
fn open_configured_source(
    radio: &RadioConfig,
    cli: &Cli,
    settings: &Settings,
) -> anyhow::Result<(Box<dyn IqSource>, DeviceCaps)> {
    match radio.backend {
        Backend::Cat => open_cat_source(radio),
        Backend::Hpsdr => open_hpsdr_source(radio, cli.freq),
        Backend::Tci => open_tci_source(radio, cli.freq),
        Backend::RtlSdr => open_rtlsdr_source(radio, cli.freq),
        Backend::Flex => open_flex_source(radio, cli.freq),
        Backend::Icom => open_icom_source(radio, cli.freq),
        Backend::Soapy => open_soapy_source(cli, settings),
        Backend::Auto => {
            #[cfg(feature = "soapy")]
            {
                let filter = device_filter(cli, settings);
                if enumerate_devices(&filter).map(|d| d.is_empty()).unwrap_or(true) {
                    open_cat_source(radio)
                } else {
                    open_soapy_source(cli, settings)
                }
            }
            #[cfg(not(feature = "soapy"))]
            {
                open_cat_source(radio)
            }
        }
    }
}

/// Open the first available SoapySDR device (feature-gated).
#[cfg(feature = "soapy")]
fn open_soapy_source(
    cli: &Cli,
    settings: &Settings,
) -> anyhow::Result<(Box<dyn IqSource>, DeviceCaps)> {
    let rate = cli.rate.unwrap_or(settings.sample_rate);
    let filter = device_filter(cli, settings);
    let devices = enumerate_devices(&filter).context("SoapySDR enumeration failed")?;
    let Some(info) = devices.first() else {
        bail!("no SoapySDR devices found (filter: {:?})", filter);
    };
    let dev =
        SoapyDevice::open(&info.args).with_context(|| format!("opening device {}", info.label))?;
    let caps = dev.caps().clone();
    Ok((Box::new(dev.rx_source(rate, cli.freq, cli.gain)?), caps))
}

#[cfg(not(feature = "soapy"))]
fn open_soapy_source(
    _cli: &Cli,
    _settings: &Settings,
) -> anyhow::Result<(Box<dyn IqSource>, DeviceCaps)> {
    bail!("SoapySDR support is not compiled into this build")
}

/// Build the CAT + sound-card source and its capabilities from radio.json.
fn open_cat_source(radio: &RadioConfig) -> anyhow::Result<(Box<dyn IqSource>, DeviceCaps)> {
    let src = audio_cat_source::AudioCatSource::open(
        radio.cat.clone(),
        radio.radio_audio_in.as_deref(),
        radio.radio_audio_out.as_deref(),
    )
    .context("opening CAT rig")?;
    let caps = cat_caps(radio);
    Ok((Box::new(src), caps))
}

/// Build the HPSDR (ethernet SDR) source from radio.json. The target IP is the
/// manual override, else the persisted selection, else the first device found by
/// a discovery scan; the protocol is detected when the connection opens.
fn open_hpsdr_source(
    radio: &RadioConfig,
    center_hz: f64,
) -> anyhow::Result<(Box<dyn IqSource>, DeviceCaps)> {
    let ip: std::net::Ipv4Addr = if let Some(s) = radio.hpsdr.target_ip() {
        s.trim().parse().with_context(|| format!("invalid HPSDR IP address {s:?}"))?
    } else {
        let found = sdroxide_hpsdr::discover_default();
        let dev = found.iter().find(|d| d.supported()).ok_or_else(|| {
            anyhow::anyhow!("no HPSDR device found on the network — enter a target IP in Settings")
        })?;
        dev.ip.parse().with_context(|| format!("discovered HPSDR IP {:?}", dev.ip))?
    };

    let src = hpsdr_source::HpsdrSource::open(ip, &radio.hpsdr, center_hz)
        .context("opening HPSDR device")?;
    let caps = hpsdr_caps(src.board(), src.sample_rate_hz(), src.protocol(), src.has_lna_gain());
    Ok((Box::new(src), caps))
}

/// Capabilities for an HPSDR board: wideband IQ (not `audio_mode`), TX-capable,
/// half-duplex. The board enforces its own limits. Protocol 1 boards top out at
/// 384 kHz, and a Hermes-Lite 2 samples at 76.8 MHz, so its Nyquist limit is
/// 38.4 MHz — tuning past that on one only aliases.
fn hpsdr_caps(board: &str, sample_rate: f64, protocol: u8, has_lna: bool) -> DeviceCaps {
    let hermes_lite = sdroxide_hpsdr::board_has_lna_gain(board);
    let nyquist = if hermes_lite { 38_400_000.0 } else { 61_440_000.0 };
    let gains = if has_lna {
        vec![sdroxide_types::GainElement {
            name: sdroxide_hpsdr::LNA_GAIN_ELEMENT.into(),
            direction: sdroxide_types::Direction::Rx,
            min_db: sdroxide_hpsdr::LNA_GAIN_MIN_DB,
            max_db: sdroxide_hpsdr::LNA_GAIN_MAX_DB,
            step_db: 1.0,
        }]
    } else {
        Vec::new()
    };
    DeviceCaps {
        driver: "hpsdr".into(),
        label: format!("{board} (HPSDR P{protocol}, {:.3} Msps)", sample_rate / 1e6),
        rx_channels: 1,
        tx_channels: 1,
        audio_mode: false,
        freq_ranges_rx: vec![(0.0, nyquist)],
        freq_ranges_tx: vec![(1_800_000.0, if hermes_lite { 30_000_000.0 } else { 54_000_000.0 })],
        sample_rates: sdroxide_types::HpsdrConfig::rates_for(protocol).to_vec(),
        gains,
        ..DeviceCaps::default()
    }
}

/// Build the RTL-SDR source from radio.json. The dongle is picked by USB
/// serial, or the first one found when none is configured.
fn open_rtlsdr_source(
    radio: &RadioConfig,
    center_hz: f64,
) -> anyhow::Result<(Box<dyn IqSource>, DeviceCaps)> {
    let src = rtlsdr_source::RtlSdrSource::open(&radio.rtlsdr, center_hz)
        .context("opening RTL-SDR dongle")?;
    let caps = rtlsdr_caps(&src);
    Ok((Box::new(src), caps))
}

/// Capabilities for an RTL-SDR: wideband IQ, receive only.
///
/// The frequency ranges are the interesting part. A Blog V4 upconverts HF in
/// hardware, so it is continuous from DC. Anything else reaches HF only through
/// direct sampling, which tops out at the ADC's Nyquist limit — leaving a gap
/// between there and the tuner's 24 MHz floor. Overlapping or disjoint ranges
/// are both fine: `DeviceCaps::can_rx_hz` is an `any` over the list.
fn rtlsdr_caps(src: &rtlsdr_source::RtlSdrSource) -> DeviceCaps {
    let rate = src.sample_rate_hz();
    let freq_ranges_rx = if src.is_blog_v4() {
        vec![(0.0, 1_766_000_000.0)]
    } else if src.hf_capable() {
        vec![(0.0, 14_400_000.0), (24_000_000.0, 1_766_000_000.0)]
    } else {
        vec![(24_000_000.0, 1_766_000_000.0)]
    };
    DeviceCaps {
        driver: "rtlsdr".into(),
        label: format!("{} ({}, {:.3} Msps)", src.describe(), src.tuner(), rate / 1e6),
        rx_channels: 1,
        tx_channels: 0,
        audio_mode: false,
        freq_ranges_rx,
        sample_rates: sdroxide_types::RtlSdrConfig::SAMPLE_RATES.to_vec(),
        gains: vec![sdroxide_types::GainElement {
            name: sdroxide_types::RtlSdrConfig::TUNER_GAIN_ELEMENT.into(),
            direction: sdroxide_types::Direction::Rx,
            min_db: 0.0,
            max_db: sdroxide_types::RtlSdrConfig::GAIN_MAX_DB,
            // The hardware only has 29 discrete steps; a request is snapped to
            // the nearest and reported back, so a fine slider is honest enough.
            step_db: 0.1,
        }],
        ..DeviceCaps::default()
    }
}

/// Build the TCI (WebSocket) source from radio.json: wideband IQ receive +
/// audio transmit.
fn open_tci_source(
    radio: &RadioConfig,
    center_hz: f64,
) -> anyhow::Result<(Box<dyn IqSource>, DeviceCaps)> {
    let src =
        tci_source::TciSource::open(&radio.tci.address, radio.tci.iq_sample_rate_hz, center_hz)
            .context("connecting to TCI server")?;
    let caps = tci_caps(&radio.tci.address, src.sample_rate_hz());
    Ok((Box::new(src), caps))
}

/// Capabilities for a TCI rig: wideband IQ RX (not `audio_mode`), TX via raw
/// audio (`tx_audio`) which the rig modulates. The rig enforces its own limits.
fn tci_caps(address: &str, iq_rate: f64) -> DeviceCaps {
    DeviceCaps {
        driver: "tci".into(),
        label: format!("TCI {address} ({:.0} kHz IQ)", iq_rate / 1000.0),
        rx_channels: 1,
        tx_channels: 1,
        audio_mode: false,
        tx_audio: true,
        freq_ranges_rx: vec![(0.0, 160_000_000.0)],
        freq_ranges_tx: vec![(1_800_000.0, 54_000_000.0)],
        sample_rates: sdroxide_types::TciConfig::IQ_RATES.to_vec(),
        // No RX gains: the SunSDR2DX ATT/Preamp is not reachable over TCI
        // (verified against ExpertSDR3 — no command spelling drives it, and
        // toggling it in the GUI emits nothing on the wire). TCI gain control
        // is deferred until a controllable path is found.
        ..DeviceCaps::default()
    }
}

/// Build the FlexRadio (SmartSDR) source from radio.json: wideband DAX IQ
/// receive + DAX audio transmit. The target IP is the manual override, else the
/// persisted selection, else the first radio a discovery listen turns up.
fn open_flex_source(
    radio: &RadioConfig,
    center_hz: f64,
) -> anyhow::Result<(Box<dyn IqSource>, DeviceCaps)> {
    let ip: std::net::Ipv4Addr = if let Some(s) = radio.flex.target_ip() {
        s.trim().parse().with_context(|| format!("invalid FlexRadio IP address {s:?}"))?
    } else {
        let found = sdroxide_flex::discover_default();
        let dev = found.first().ok_or_else(|| {
            anyhow::anyhow!(
                "no FlexRadio found on the network — enter a radio IP in Settings → Radio"
            )
        })?;
        dev.ip.parse().with_context(|| format!("discovered FlexRadio IP {:?}", dev.ip))?
    };

    let src = flex_source::FlexSource::open(ip, &radio.flex, center_hz)
        .context("connecting to the FlexRadio")?;
    // Remember the identity the radio gave us. It keeps a GUI client's slices
    // and panadapters per client id, so coming back under the same one is what
    // hands our objects back instead of stranding them — a fresh id every start
    // eats one of the radio's few slices each time.
    let id = src.client_id();
    if !id.is_empty() && radio.flex.client_id.as_deref() != Some(id) {
        let mut cfg = radio.clone();
        cfg.flex.client_id = Some(id.to_string());
        if let Err(e) = sdroxide_config::save_radio_config(&cfg) {
            tracing::warn!("saving the FlexRadio client id: {e}");
        }
    }
    let caps = flex_caps(
        src.model(),
        &ip.to_string(),
        src.sample_rate_hz(),
        src.rf_gain_range(),
        src.has_atu(),
    );
    Ok((Box::new(src), caps))
}

/// Capabilities for a FlexRadio: wideband IQ RX (not `audio_mode`), TX via DAX
/// audio (`tx_audio`) which the radio modulates. RX coverage is the 6000/8000
/// series' 30 kHz–165 MHz; the radio enforces its own transmit limits.
fn flex_caps(
    model: &str,
    ip: &str,
    iq_rate: f64,
    rf_gain: Option<(f64, f64, f64)>,
    has_atu: bool,
) -> DeviceCaps {
    // RX gain is the panadapter's `rfgain` — the preamp/attenuator ahead of the
    // converter, and the only gain of the radio's that changes the DAX IQ we
    // receive (its AGC sits in the slice, downstream of our tap). The steps are
    // whatever the radio named; a radio that named none simply has no slider.
    let gains = rf_gain
        .map(|(min_db, max_db, step_db)| {
            vec![sdroxide_types::GainElement {
                name: flex_source::RF_GAIN.into(),
                direction: sdroxide_types::Direction::Rx,
                min_db,
                max_db,
                step_db,
            }]
        })
        .unwrap_or_default();
    DeviceCaps {
        driver: "flex".into(),
        label: format!("{model} @ {ip} ({:.0} kHz DAX IQ)", iq_rate / 1000.0),
        rx_channels: 1,
        tx_channels: 1,
        audio_mode: false,
        tx_audio: true,
        freq_ranges_rx: vec![(30_000.0, 165_000_000.0)],
        freq_ranges_tx: vec![(1_800_000.0, 54_000_000.0)],
        sample_rates: sdroxide_types::FlexConfig::IQ_RATES.to_vec(),
        gains,
        has_atu,
        ..DeviceCaps::default()
    }
}

/// Build the Icom network source: CI-V and audio over UDP, no cable in between.
fn open_icom_source(
    radio: &RadioConfig,
    center_hz: f64,
) -> anyhow::Result<(Box<dyn IqSource>, DeviceCaps)> {
    let src =
        icom_source::IcomSource::open(&radio.icom, center_hz).context("connecting to the Icom")?;
    let caps = icom_caps(src.model(), &radio.icom.ip);
    Ok((Box::new(src), caps))
}

/// Capabilities for an Icom on the network: demodulated audio (`audio_mode`),
/// so the panadapter shows the audio band rather than a wideband spectrum, and
/// transmit by streaming audio the radio modulates.
fn icom_caps(model: &str, ip: &str) -> DeviceCaps {
    DeviceCaps {
        driver: "icom".into(),
        label: format!("{model} @ {ip} (network)"),
        rx_channels: 1,
        tx_channels: 1,
        audio_mode: true,
        freq_ranges_rx: vec![(30_000.0, 470_000_000.0)],
        freq_ranges_tx: vec![(1_800_000.0, 450_000_000.0)],
        ..DeviceCaps::default()
    }
}

/// Capabilities for a CAT rig. TX-capable unless PTT is VOX-only-with-no-audio;
/// we advertise TX so the UI shows PTT and the safety rails apply. Frequency
/// range covers HF+6m (the rig enforces its own limits over CAT).
fn cat_caps(radio: &RadioConfig) -> DeviceCaps {
    let demod = matches!(radio.cat.format, sdroxide_types::SoundFormat::DemodAudio);
    DeviceCaps {
        driver: "cat".into(),
        label: format!("{} (CAT)", radio.cat.family.label()),
        rx_channels: 1,
        tx_channels: 1,
        audio_mode: demod,
        freq_ranges_rx: vec![(100_000.0, 148_000_000.0)],
        freq_ranges_tx: vec![(1_800_000.0, 54_000_000.0)],
        ..DeviceCaps::default()
    }
}

/// Capabilities for non-hardware sources (RX-only, unlimited tuning).
fn synthetic_caps(label: &str) -> DeviceCaps {
    DeviceCaps {
        driver: "none".into(),
        label: label.into(),
        rx_channels: 1,
        tx_channels: 0,
        freq_ranges_rx: vec![(0.0, 6e9)],
        ..DeviceCaps::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(oob: bool) -> Cli {
        let mut c = Cli::parse_from(["sdroxide"]);
        c.oob_tx = oob;
        c
    }

    /// `--oob-tx` may only ever *loosen* the lockout. A build that never passes
    /// it has to behave exactly as it did before the flag existed, and no
    /// combination of flags may turn the lockout on when the config has turned
    /// it off — that would be a surprise in the dangerous direction.
    #[test]
    fn the_flag_only_ever_loosens_the_band_lockout() {
        let locked = Settings { tx_ham_only: true, ..Settings::default() };
        let open = Settings { tx_ham_only: false, ..Settings::default() };

        assert!(cli(false).tx_ham_only(&locked), "the default must keep the lockout");
        assert!(!cli(true).tx_ham_only(&locked), "--oob-tx must lift it");
        // Already unlocked in the config: the flag changes nothing either way.
        assert!(!cli(false).tx_ham_only(&open));
        assert!(!cli(true).tx_ham_only(&open));
    }

    /// The flag is opt-in on the command line and nowhere else.
    #[test]
    fn the_flag_is_off_unless_asked_for() {
        assert!(!Cli::parse_from(["sdroxide"]).oob_tx);
        assert!(Cli::parse_from(["sdroxide", "--oob-tx"]).oob_tx);
        assert!(Settings::default().tx_ham_only, "the shipped default is locked");
    }
}
