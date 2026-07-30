# sdroxide

A PowerSDR/Thetis-style software-defined-radio transceiver client in Rust, with
pluggable radio backends (**SoapySDR**, **OpenHPSDR**, **TCI**, and **CAT**), an
[egui](https://github.com/emilk/egui) GUI, and a cyberpunk theme. It runs as a **native desktop application** and, from the same
binary, as a **server that streams the same UI to a web browser** over
WebSocket. It includes an integrated, persistent **logbook**, full **FT8/FT4**
digital-mode operation, and **built-in TCI and Hamlib rigctld servers** so
third-party programs like WSJT-X can use it as their radio.

<img width="2351" height="984" alt="image" src="https://github.com/user-attachments/assets/6130eb56-9486-414c-b4b8-ceeb366d812c" />

<img width="2422" height="984" alt="image" src="https://github.com/user-attachments/assets/2b502c2b-f37c-43be-9eb9-e55eaea04419" />



> ## [User Manual](docs/USER_MANUAL.md)

One binary, three ways to run it:

- **Native** — a local desktop transceiver against your SDR hardware.
- **Server** — `sdroxide --server`; the DSP runs on the machine with the radio
  and the full UI (plus audio and the waterfall) is served to a browser as
  WebAssembly. One remote client at a time.
- **Native remote** — `sdroxide --connect host:4950`; the desktop UI driving a
  remote server instead of local hardware.

## Core features

- **Panadapter** — GPU (wgpu) waterfall + spectrum line, wheel-zoom around the
  cursor, drag-to-pan, per-digit frequency readout, selectable colormaps,
  peak-hold, and a **one-click auto-contrast** ("FIT") that picks the display
  floor/ceiling from the signals currently on screen.
- **Bandplan overlay** — a colour-coded strip along the bottom of the waterfall
  that labels allocations (ham bands, broadcast, CB, AM); it shows coarse bands
  when zoomed out and CW/digital/SSB sub-segments when zoomed into a ham band.
- **Modes** — SSB (USB/LSB), CW, AM, SAM, NFM, WFM, DSB, DIGU/DIGL, a
  spectrum-only mode, **FT8/FT4**, the keyboard modes **PSK31**, **RTTY**,
  **Olivia**, **THOR** and **FSQ** (with directed messaging + images), image
  **SSTV** (Scottie, Martin, Robot), and transmit-only **RF Paint** (spectrum
  painting of text and images onto the waterfall).
- **Receiver** — hang AGC, draggable passband filter edges (on the spectrum and
  the waterfall), noise blanker, auto-notch, **neural (RNNoise) or spectral noise
  reduction**, squelch, a second sub-receiver, RIT/XIT, VFO A/B with split,
  per-band band stacks, and memory channels.
- **Transmit** — PTT and tune carrier, drive/ALC metering, device-aware
  half-duplex sequencing (HackRF) or full-duplex (LimeSDR), and a ham-band /
  TX-range lockout so you can't key outside your allocation.
- **Resizable layout** — drag the frequency-scale strip to resize the spectrum
  vs. waterfall split; in FT8/FT4, drag the divider to resize the operating
  panel.
- **Live spotting, awards & QSL** — DX cluster / POTA / SOTA / PSK Reporter spots
  as clickable panadapter markers (click to tune + pre-fill a log entry),
  QRZ/HamQTH callsign lookup, one-click upload to LoTW / eQSL / Club Log / QRZ,
  and live **DXCC / WAS / WAZ / grid** award tracking (worked vs confirmed).
- **Control inputs** — every shortcut is rebindable, and any class-compliant
  **MIDI controller** can drive the radio: a jog wheel as the VFO knob, pads as
  PTT and band buttons, faders as gain controls, with LED/motor feedback. Mouse
  buttons take bindings too (a side button held for PTT works as a footswitch),
  and the panadapter wheel can zoom or tune.
- **Persistence** — device, rates, gains, memories, band stacks, the FT8/FT4
  operator profile, network/QSL credentials, control bindings, and the logbook
  are all stored under `~/.config/sdroxide/`.

## FT8 / FT4

<img width="1683" height="933" alt="image" src="https://github.com/user-attachments/assets/02a4b70d-7590-4a71-aacb-56814132b691" />

Selecting FT8 or FT4 switches the panadapter to a zoomed sub-band waterfall with
a decode list and an auto-sequencing QSO panel:

- Click a decoded line to move your TX audio frequency onto that signal (a faint
  marker appears on the world map); press **REPLY** to start an auto-sequenced
  QSO, or **Call CQ** to call.
- A dot-matrix **world map** shows your grid, the station you're working, and an
  animated pulse travelling the great-circle path while you transmit.
- Own callsign, grid, and message templates are set in the FT8/FT4 setup dialog
  and persisted.
- All decoding and encoding run server-side in the native engine, so native and
  browser clients behave identically.

## PSK31 and RTTY

Selecting **PSK** or **RTTY** opens a live keyboard-mode ragchew panel next to a
zoomed sub-band waterfall — tune onto a signal, watch it decode, and type a
reply that transmits as you type:

- **Receive** streams decoded text into a scrolling window. Fine-tune with the
  **−/+** buttons (±10 Hz) onto the carrier; RTTY draws mark/space tuning lines
  on the waterfall.
- **Transmit** as you type: characters already sent turn **green** so you can
  watch the transmission catch up to your typing. **TX** keys/unkeys, **CALL CQ**
  loads and sends a CQ macro, **CLEAR** empties the buffer.
- **PSK** is BPSK31 (differential BPSK, varicode). **RTTY** defaults to 45.45
  baud / 170 Hz shift / Baudot; shift (170/425/850 Hz) and baud (45/50/75) are
  selectable in the PSK/RTTY setup dialog.
- The **PSK and RTTY skimmers** decode signals across each band's PSK/RTTY
  calling sub-bands and label them on the waterfall; click a label to switch to
  that mode, tune onto it, and open the panel.

## Olivia, THOR and FSQ

Three more keyboard modes share the same ragchew panel and setup dialog as
PSK/RTTY; the submode is chosen on each mode's setup page:

- **Olivia** — a very robust MFSK chat mode with Walsh/Hadamard coding. Pick the
  tone count (2–64) and bandwidth (125–2000 Hz); 32/1000 and 16/500 are common.
- **THOR** — DominoEX-family 18-tone incremental-FSK with convolutional FEC.
  Pick a submode (THOR4 … THOR32; THOR16 is the usual default).
- **FSQ** — 33-tone incremental-FSK (speeds FSQ-2/3/4.5/6) with a dedicated panel
  for the **directed (FSQCALL)** layer: a **heard list**, a persistent **contacts**
  book, directed `CALL:message` sends, ALLCALL broadcast, an automatic reply to
  the `?` heard-list query, and **image** transmit/receive (pick a picture to send;
  received pictures land in the gallery).

These modems are native-Rust and self-contained (no external decoder); on-air
interoperability with fldigi is being validated and refined.

## SSTV

Selecting **SSTV** opens an image panel with a received-image gallery on the
left and a transmit compositor on the right:

- **Receive** decodes incoming pictures scanline-by-scanline into the gallery;
  the VIS header sets the mode automatically (and pre-selects it for your next
  transmit). Received images are saved under `~/.config/sdroxide/sstv_rx/`.
- **Transmit** from a strip of five image slots — click to select, double-click
  (or click an empty slot) to pick a file, which is auto-cropped/scaled to the
  mode's size. A multi-line message is overlaid on the image, **each line in a
  different font**, bold with a black outline; a live preview shows exactly what
  will be sent. Every transmitted image carries a small red→black header strip
  with "SDRoxide" and the version. **TX** sends; **ABORT TX** stops.
- **Modes:** Scottie 1 / 2 / DX, Martin 1 / 2, Robot 72, Robot 36. Band buttons
  tune to that band's SSTV calling frequency (e.g. 20 m = 14.230 MHz).

## RF Paint

Selecting **RFPAINT** opens a transmit-only **spectrum-painting** panel that draws
text and pictures **directly onto a receiver's waterfall** — there is no decoder,
the picture *is* the signal, so anyone watching their panadapter on your frequency
simply sees what you paint. It transmits on USB inside a 3 kHz audio band, so it
fits a normal SSB channel:

- **Text paint** — type a line and it is rendered as upright letters that scroll
  up the far station's waterfall (constant font size — a longer message just makes
  a wider banner / longer transmission).
- **Image paint** — load a PNG/JPEG, reduced to a contrast-stretched grayscale
  bitmap and painted onto the waterfall.
- Each area has a **live preview waterfall** showing exactly how it will look on
  the receiving end, plus a **TRANSMIT** button, a transmit-progress bar, and
  **Abort**.
- A **scan-speed** control (≈6%–100%, default 25%) trades transmission time for
  legibility — slower gives the receiver's waterfall more scan lines to render
  the picture. Transmit goes through the normal path, so the ham-band lockout and
  transmit safety still apply.

## RADE digital voice

Selecting **RADE** switches the receiver to **FreeDV RADE V1** (Radio
Autoencoder) — a neural speech codec carried on an OFDM waveform, which stays
intelligible at signal-to-noise ratios where SSB is just noise. It fits inside a
normal USB channel, occupying roughly 1060–1880 Hz of audio.

- **Receive** replaces the demodulated audio with the decoded speech as soon as
  the modem locks. Out of sync you still hear the raw signal, so you can tune by
  ear; the panel shows a sync lamp, the SNR estimate and the frequency offset,
  and the waterfall is marked with the band the waveform occupies.
- **Transmit** with the panel's **TALK** button or the ordinary PTT. The modem 
  needs ~120 ms of speech before the first frame goes out and appends an 
  end-of-over frame when you stop, so transmit runs on slightly past the button.
- Band buttons tune to the FreeDV calling frequencies (e.g. 20 m = 14.236 MHz).
- Decoding is neural-network inference and runs on its own thread; it is far
  faster than real time on a modern CPU, but the panel warns if the machine
  falls behind.

`rade-harness` (in `crates/sdroxide-rade`) drives the same codec over files, for
bench testing without a radio:

```sh
cargo run -p sdroxide-rade --bin rade-harness -- \
    tx --input vendor/rade_c/wav/david_vk5dgr.wav --output modem8k.wav
cargo run -p sdroxide-rade --bin rade-harness -- \
    rx --input modem8k.wav --speech decoded16k.wav --stats rx.csv
```

## Logbook

Open the **LOG** button (available in any mode) for a persistent logbook that
holds both FT8/FT4 and manually entered QSOs:

- Entries are grouped into daily **sessions** with a time span and QSO count.
- **+ New Entry** adds a manual QSO. Alongside the basics (call, frequency, mode,
  RST, grid, UTC date/time) the entry form now carries **name, QTH, state,
  country**, transmit **power**, and **contest** fields (contest id + sent/received
  serials); a **worked-before** badge warns when you've already worked that call
  on the band. **EDIT** and **DEL** amend or remove any past entry.
- FT8/FT4 QSOs are logged automatically as they complete.
- **IMPORT** loads QSOs from an ADIF (`.adi`) file (de-duplicated against the
  existing log); export the whole book to **ADIF** or plain **TXT**. A
  QSL/confirmation status column shows what's been uploaded and confirmed.
- Records also hold DXCC entity, CQ/ITU zones, IOTA and POTA/SOTA references, and
  per-service QSL status — the data behind lookup, upload and award tracking.
- The log is stored at `~/.config/sdroxide/qso_log.json` (native) or in browser
  storage (remote).

## Spotting, awards & QSL upload

Turn the logbook into a live station cockpit. Everything here is configured on
the **Spots** and **Uploads** tabs of the Settings dialog, and surfaced by the
**SPOTS** and **AWARDS** buttons in the System module.

![Live spots as clickable markers on the panadapter, and the SPOTS window](docs/images/14-spots-panel.png)

- **Spot feeds** — connect a **DX cluster** (telnet) and poll **POTA**, **SOTA**
  and **PSK Reporter**. Spots appear as clickable, colour-coded markers along the
  bottom of the waterfall (and as dots on the FT8 world map); the **SPOTS** window
  lists them with per-source filters. **Click a spot** to tune the VFO, set the
  mode, and pre-fill a new log entry — one click from "heard" to "working".
- **Callsign lookup** — auto-fill name, QTH, grid and state from **QRZ.com** or
  **HamQTH** on a spot click, at QSO start, or when you type a call (or press
  **LOOKUP** in the entry form).
- **One-click upload** — push QSOs to **eQSL**, **QRZ Logbook** and **Club Log**
  (a per-QSO **UP** button, or automatically as each QSO is logged). **LoTW** is
  handled by exporting ADIF for TQSL signing; LoTW/eQSL **confirmations are
  downloaded** to mark worked-vs-confirmed.
- **Award tracking** — the **AWARDS** window tallies **DXCC**, **WAS**, **WAZ**
  and **grid squares**, worked vs confirmed, with a per-band filter. DXCC entity
  and CQ/ITU zones are resolved from the callsign (bundled `cty.dat`), so spots
  for a **new entity** are flagged in the SPOTS list.

Credentials are stored in plaintext under `~/.config/sdroxide/net.json` (as with
other ham software). See the [User Manual](docs/USER_MANUAL.md) for setup steps.

## Radio backends

sdroxide can drive six kinds of radio, selected on the **Radio** tab of the
Settings window. Backend, serial, and radio-audio changes apply live when you
press **Apply / reconnect**. A radio that isn't there yet at startup — or that
drops mid-session — is retried in the background and attaches by itself, so
starting sdroxide before the rig is fine:

- **SoapySDR** — any [SoapySDR](https://github.com/pothosware/SoapySDR) device
  (wideband IQ). See below.
- **OpenHPSDR** — Hermes/Metis-family Ethernet SDRs on the LAN (Protocol 1 and
  2). Press **Discover** to scan for devices, or enter the IP manually; pick a
  DDC sample rate (48 kHz–1536 kHz). Not yet hardware-verified — testers can run
  `RUST_LOG=sdroxide_hpsdr=debug sdroxide` for connection/RX diagnostics (see the
  user manual, §5.4).
- **CAT / Audio** — a CAT-controlled rig (Icom/CI-V, Yaesu, Xiegu) with audio
  over a USB sound card, as either demodulated mono audio or stereo IQ.
- **TCI** — a TCI (Transceiver Control Interface) server such as ExpertSDR3 
  over WebSocket (default `127.0.0.1:50001`): wideband IQ receive plus 
  audio transmit.
- **FlexRadio** — a FLEX-6000 or FLEX-8000 over the SmartSDR API, with no
  SmartSDR, DAX or SmartCAT installed: press **Discover** to find radios on the
  LAN, pick a DAX IQ rate (24–192 kHz) and channel. sdroxide connects as a GUI
  client and creates its own panadapter, slice and streams — wideband DAX IQ
  receive, DAX audio transmit, forward power and SWR from the radio's meters,
  the panadapter's RF gain, an **AGC-T** slider next to the AGC menu, and an
  **ATU** button with its Success/Bypass readout on radios fitted with a tuner.

- **Icom (network)** — an IC-705, IC-7610 or IC-9700 over LAN or WLAN, speaking
  the protocol Icom's own RS-BA1 uses: CI-V control and receive/transmit audio
  over UDP. No cable, no sound card and no wfview or virtual COM port in
  between. Enable network control on the radio and give sdroxide the same
  username and password. The waterfall is the radio's own spectrum scope
  (±2.5 kHz to ±500 kHz, and the radio's SPAN button moves it); audio is the
  demodulated receiver output, since the protocol carries no IQ.

The wideband-IQ backends (SoapySDR, HPSDR, TCI, FlexRadio) drive the full
panadapter, the CW/PSK/RTTY skimmers, and internal demodulation; a CAT rig
feeding demodulated audio shows only a narrow audio-band slice.

## Built-in TCI server

sdroxide is also a **TCI server**, so TCI-capable programs — WSJT-X's SunSDR
(TCI) rig type, JTDX, MSHV, skimmers — can use it as their radio: frequency and
mode control, a wideband IQ stream, receive audio to decode, and transmit audio
to put on the air. Several clients can connect at once.

It is on by default at `127.0.0.1:50001` and configured on the **Servers** tab
of the Settings dialog, which also shows the live client count. TCI has no
authentication, so it listens on localhost only unless you change that; the
transmitter has a single owner, and keying up locally always takes it back.
Verified against WSJT-X (rig *TCI Client RX1*, PTT via CAT, TCI audio). See the
user manual, §5.6.

## Built-in Hamlib rigctld server

Most amateur software reaches a radio through **Hamlib**, over the network
protocol its `rigctld` daemon speaks. sdroxide serves that protocol directly, so
**WSJT-X, fldigi, JS8Call, N1MM, Log4OM, GPredict and CQRLOG** can drive it with
no extra daemon, no serial cable and no virtual COM port pair — frequency, mode
and passband, PTT, VFO A/B and split, RIT/XIT, power and volume levels, the
NB/NR/ANF/MUTE functions, and the VFO operations.

It is **off by default** — port 4532 is often already held by a real `rigctld`,
and the protocol has no authentication — and lives on the **Servers** tab next
to the TCI server. In WSJT-X or fldigi choose the rig **Hamlib NET rigctl**
(model 2) and point it at `127.0.0.1:4532`. Unlike TCI it carries control only,
no audio or IQ; both servers can run at once. See the user manual, §5.10.

## Control inputs

Every keyboard shortcut is a rebindable **action**, and the same action list is
reachable from mouse buttons and from a MIDI controller — the cheapest real VFO
knob there is. Configured on the **Controls** tab; see the user manual, §5.9.

Push-to-talk ships **unbound** on purpose. One click binds hold-to-talk to
Space, and a held PTT is released on key-up, on window focus loss, on a text
field taking the keyboard, when the controller is unplugged, and after a
configurable timeout.

Bindings are stored with the *user interface*, not the engine, so a knob plugged
into your laptop works against a remote radio over `--connect` too.

## SoapySDR connectivity

sdroxide talks to any [SoapySDR](https://github.com/pothosware/SoapySDR) device.
It has been developed against a **HackRF One** (half-duplex TX) and a
**LimeSDR** (full-duplex TX).

- Select a device with `--device`, using SoapySDR argument syntax, e.g.
  `--device driver=hackrf` or `--device driver=lime,serial=...`. With no
  argument it uses the configured device, else the first one found.
- `sdroxide --probe` lists all detected devices and their probed capabilities
  (frequency and sample-rate ranges, gains, antennas, sensors, duplex) and
  exits.
- Capabilities drive the UI: RX-only devices hide all TX controls, band buttons
  grey out outside the device's tunable range, and SWR/power meters appear only
  when the device exposes those sensors.
- Hardware-free sources for testing: `--siggen` (built-in signal generator) and
  `--file <raw CF32 IQ>`.

## Building

The RADE digital-voice codec is vendored as a git submodule, so clone with:

```sh
git clone --recurse-submodules https://github.com/dividebysandwich/sdroxide
# or, in an existing checkout:
git submodule update --init --recursive
```

You need the SoapySDR development libraries and the driver module(s) for your
radio installed (e.g. `soapysdr`, `soapysdr-module-hackrf`,
`soapysdr-module-lms7` on Arch/Debian-style distros).

Building RADE additionally needs **CMake**, a **C compiler**, **libclang**
(for `bindgen`) and **autoconf / automake / libtool** — its build fetches and
compiles a FARGAN-enabled Opus from source. That fetch means the *first* build
needs network access; later builds reuse it. It is also the slow part of a clean
build: RADE's model weights are ~110 MB of generated C.

```sh
cargo build --release
./target/release/sdroxide --probe        # verify your device is seen
```

The browser client is a separate WebAssembly crate built with
[Trunk](https://trunkrs.dev/):

```sh
cd crates/sdroxide-web && trunk build --release
```

Build the server with `--features embed-web` to bake the web client into the
binary so `--server` needs no `--web-root`.

## Running

```sh
# Native desktop, tuned to 20 m, FT8:
sdroxide --freq 14074000 --mode ft8

# Server: DSP + hardware here, UI in a browser at http://<host>:4950
sdroxide --server

# Desktop UI driven by a remote server:
sdroxide --connect 192.168.1.10:4950
```

## Startup parameters

| Flag | Description |
| --- | --- |
| `--device <ARGS>` | SoapySDR device args (e.g. `driver=hackrf`). Default: config, then first device found. |
| `--probe` | List devices and their probed capabilities, then exit. |
| `--console` | Terminal (ASCII) waterfall mode, no GUI. |
| `--siggen` | Use the built-in signal generator instead of hardware. |
| `--file <FILE>` | Play a raw interleaved CF32 IQ file instead of hardware. |
| `--freq <HZ>` | Center frequency in Hz (default `14200000`). |
| `--rate <HZ>` | Sample rate in Hz (default: from config). |
| `--gain <DB>` | Overall RX gain in dB (default: hardware AGC / moderate). |
| `--mode <MODE>` | Initial mode: `USB LSB CW AM SAM NFM WFM DIGU DIGL DSB SPEC FT8 FT4 PSK RTTY OLIVIA THOR FSQ SSTV RFPAINT RADE`. |
| `--server` | Run as a server: HTTP web client + WebSocket streaming backend. |
| `--connect <HOST[:PORT]>` | Connect as a native remote client to a running server. |
| `--port <PORT>` | Server port (default: from config, `4950`). |
| `--web-root <DIR>` | Directory with the Trunk-built web client (default: embedded assets with `--features embed-web`). |
| `--fft <N>` | Spectrum FFT size (default `4096`). |
| `--tx-tune <SECS>` | Headless TX smoke test: key a tune carrier at minimal drive, then exit. |
| `--ft8-cq <SECS>` | Headless FT8 smoke test: call CQ at minimal power, then exit. |
| `--rade-rx <SECS>` | Headless RADE smoke test: receive for SECS seconds and report whether the modem synced. Pair with `--file`. |
| console extras | `--fps <N>` lines/sec, `--width <CHARS>`, `--db-floor <dBFS>`, `--db-ceil <dBFS>`. |

## Keyboard shortcuts

Active whenever a text field isn't focused. These are the **defaults** — all of
them, plus PTT, band, mode, filter and much else, are rebindable on the
**Controls** tab.

| Key | Action |
| --- | --- |
| `←` / `→` | Tune ∓/± 100 Hz (hold **Shift** for 10 Hz fine steps) |
| `↑` / `↓` | Tune ± 1 kHz |
| `PageUp` / `PageDown` | Tune ± 10 kHz |
| `M` | Toggle mute |
| `N` | Toggle the noise blanker |
| `F` | Fit the panadapter to the full device passband |

## Mouse operation

**Panadapter (spectrum + waterfall)**

| Action | Result |
| --- | --- |
| Left-click | Tune the active VFO to that frequency. In FT8/FT4, sets the TX audio offset instead. |
| **Shift** + left-click | Tune VFO B (sub-receiver) to that frequency. |
| Left-drag | Grab and slide the spectrum — pans the view and tunes along with it. |
| Right-drag | Pan the view only (no tuning). |
| Scroll wheel | Zoom in/out around the cursor. |
| Drag a passband edge | Move that filter edge (works on the spectrum and the waterfall). |
| Drag the frequency-scale strip | Resize the spectrum vs. waterfall split. |
| Drag the waterfall / FT8 panel divider | Resize the FT8/FT4 operating panel. |

**Frequency readout** — scroll the wheel over a digit to step that digit; click
its upper half to increment, lower half to decrement.

**FT8/FT4 decode list** — click a row to move your TX audio onto that signal
(and preview it on the map); press **REPLY** to start an auto-sequenced QSO.


## Contributing, LLM Usage, Licensing

Both local and hosted LLMs (usually advertised as "Generative AI") were used in 
the development of this software. Contributions written using LLMs are ok 
provided the following rules are observed:

* **Read and review** generated code. You should be able to answer questions 
about your contribution.
* **Document and comment** non-trivial parts of the code.
* **Test** your contribution using real radio equipment. If this is not possible,
consider if this is a useful contribution and disclose the need for testing help
before you start.
* Don't use LLMs for trivial things like changing a constant. This is slow,  wasteful
and runs the risk of unneccessary modifications elsewhere.
* Use modern, sufficiently sized models with sufficient context size. Running 
small or outdated models or limiting them to small contexts results in low 
quality code and damage to existing functionality.
* Usage of locally-hosted LLMs is encouraged, but not required.
* Please keep commits vendor-neutral and don't commit specific files for 
one specific cloud hosted LLM.
* Observe the project license. This is a GPLv3 project. Changing the license 
would violate the terms of several of the used libraries.

