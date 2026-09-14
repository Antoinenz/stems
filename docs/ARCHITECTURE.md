# stems — System Architecture & Technical Spec

> Open-source party app: runs on a desktop (or a small box), projects to a TV,
> acts as a Spotify Connect / AirPlay receiver, renders synced lyrics + music
> videos, and does local ML vocal removal (karaoke mode).

Status: draft v0.2 · 2026-09-14 (adds AirPlay modes, rebeam/OpenAir, video prefetch, real hardware, rendering decision)

---

## 0. Guiding decisions (read this first)

| # | Decision | Why |
|---|----------|-----|
| D1 | **We are the audio sink, not a tap.** Audio enters via receivers we control (librespot / shairport-sync). We never need system-audio capture on the main path. | Raw PCM + exact metadata + timing for free; no virtual audio drivers; cross-platform capture becomes an optional fallback mode. |
| D2 | **Headless Rust core (`stemsd`) + web clients.** Stage display, host panel and guest remote are all web apps talking to the same local server. Tauri is only the desktop wrapper. | One state model, one protocol. Stage can run in Tauri, a kiosk Chromium, or a Pi plugged into the TV. Testable without a GUI. |
| D3 | **Latency is a feature, not a bug.** Being a receiver, we can add a fixed delay line (2–6 s) in front of the output. | Turns "real-time" separation into "streaming with lookahead": chunked inference on a lag buffer, which mid-size models can actually do. Sender UI position drifts ahead by the same amount — we correct the lyric clock, and tell the user. |
| D4 | **Separation is a pluggable worker.** Trait `Separator` with in-process (ONNX/`ort`), sidecar (Python/PyTorch), and remote (HTTP/WS to a GPU box on the LAN) backends, plus a zero-cost DSP fallback (centre-channel cancel). | Dev on a Pi, run the model on a GPU laptop, ship both. Users without a GPU still get *something*. |
| D5 | **Prefer pre-buffering over real-time whenever the source lets us.** Spotify (librespot) lets us fetch the full audio file ahead of playback; AirPlay does not. | Offline separation of the *next* track with a high-quality model beats streaming a low-quality one. Streaming is the AirPlay path only. |
| D6 | **Cache everything content-addressed.** Stems, lyrics, video matches, alignments keyed by ISRC / Spotify track id / fingerprint. | A party replays the same songs; second play is instant. |
| D7 | **Two AirPlay personalities.** *Display mode*: a normal AirPlay 2 receiver that honours the sender's multiroom clock. *Karaoke mode*: we play `L` late, sender-side grouping is sacrificed, and **rebeam** (OpenAir sink) re-syncs every room from *our* output. | Vocal removal is incompatible with sender-driven sync by definition; re-mastering the clock on our side is the only way every speaker plays the karaoke mix. |
| D8 | **Sinks are plural and latency-aware.** `Sink` trait: cpal local, OpenAir AirPlay 2 sender (multi-room), later Snapcast/SonoBus. Each reports latency; the mixer aligns all sinks to the slowest. | Local TV audio and rebeamed rooms stay in sync without special cases. |
| D9 | **The stage is a pure view.** It receives state + a clock over WS and draws; it never touches audio. | Makes the web/native rendering choice reversible (see §4.1). |

---

## 1. Core architecture & tech stack

### 1.1 Topology

```
                 ┌──────────────────────── stemsd (Rust) ────────────────────────┐
 phone/laptop    │                                                                │
 ──Spotify──▶  librespot ─┐                                                       │
 ──AirPlay──▶  shairport ─┼─▶ Source ─▶ RingBuf ─▶ [DelayLine] ─▶ Mixer ─▶ cpal ─▶ speakers/TV
 (fallback)    sys capture┘   (PCM +     │            ▲                              │
                              events)    │            │ stems (vocals/inst)          │
                                         ▼            │                              │
                                   Separator worker ──┘   (in-proc ONNX | sidecar | remote GPU)
                                                                                     │
   Track events ─▶ Enrichment (LRCLIB / Web API / yt-dlp / alignment) ─▶ SQLite + file cache
                                                                                     │
   State machine (NowPlaying, clock, karaoke mode, queue) ─▶ axum HTTP + WS ─▶  ┌────┴─────┐
                                                                                 │ clients  │
                                        Tauri window (stage, fullscreen on TV) ◀─┤ stage    │
                                        LAN phones (PWA, QR code on screen)    ◀─┤ remote   │
                                        host control panel                     ◀─┤ host     │
                                                                                 └──────────┘
```

### 1.2 Desktop framework: **Tauri 2 + Rust core + web front-ends**

Considered: Tauri 2, Electron, native Rust (wgpu + egui/iced), Flutter.

| Option | UI perf / typography | Local API access | Overhead | Verdict |
|---|---|---|---|---|
| **Tauri 2** | Good (WebGL2 in WKWebView / WebView2); **weak on Linux** (WebKitGTK GPU path is flaky) | Excellent — core is Rust | ~30–60 MB RAM, tiny binary | **Pick.** Linux weakness mitigated by D2: stage can run in any Chromium in kiosk mode. |
| Electron | Uniform Chromium everywhere | Node ↔ native via N-API; audio in Node is painful | 150–300 MB RAM | Reject; the audio/ML core wants to be Rust anyway. |
| wgpu native | Best possible, full control | Native | Minimal | Reject for MVP: text layout/animation (cosmic-text + glyphon) is a lot of work vs. CSS/WebGL. Keep as escape hatch for the stage window only. |
| Flutter | Good | FFI to Rust (flutter_rust_bridge) | Medium | Reject; two ecosystems. |

Because of D2, **Tauri is thin**: it spawns/links `stemsd`, opens a fullscreen stage window on the chosen display, and a host window. Everything else is HTTP/WS.

### 1.3 Repository layout (Cargo workspace)

```
stems/
  crates/
    stems-core/        # state machine, audio graph, clock, traits (Source, Separator, Sink)
    stems-audio/       # cpal output, ring buffers, delay line, resampler (rubato), DSP fallback
    stems-source-spotify/  # librespot integration (custom Sink + player events)
    stems-source-airplay/  # shairport-sync sidecar (pipe audio + metadata pipe parser)
    stems-source-capture/  # WASAPI loopback / ScreenCaptureKit / PipeWire  (Phase 3)
    stems-sep/         # Separator trait + backends: dsp, ort (ONNX), sidecar, remote
    stems-enrich/      # lyrics (LRCLIB…), video (yt-dlp), alignment, cache
    stems-server/      # axum: REST + WS, mDNS advert, static file serving
    stemsd/            # binary: wires everything, CLI flags, config
  apps/
    desktop/           # Tauri 2 shell
  web/
    stage/             # SvelteKit/Solid + PixiJS: lyrics, video, visualizer
    remote/            # mobile PWA for guests
    host/              # host control panel (can be merged with remote + role)
  models/              # model manifest + download script (never vendored)
  sidecars/
    separator-py/      # optional PyTorch worker (Demucs / Roformer)
  docs/
```

Language split: **Rust** for everything on the audio path and state; **TypeScript** for UI; **Python** only in the optional heavy-model sidecar.

### 1.4 Audio pipeline

**Format**: internal f32 interleaved stereo, 44.1 kHz (Spotify/AirPlay native). Resample with `rubato` only at device output if the device refuses 44.1k.

**Graph** (all lock-free; `ringbuf`/`rtrb` SPSC rings between threads; no allocation on the output callback):

```
Source thread  ──PCM frames + SourceEvent──▶  ingest ring (≈ 10 s)
                                                  │
Separator thread ◀───────── reads chunks ─────────┤ (only in karaoke/streaming mode)
      │ writes {vocals, inst} chunks              │
      ▼                                           ▼
 stem rings ─────────────────────▶  DelayLine(L) ─▶ Mixer(vocal_gain, inst_gain) ─▶ cpal callback
```

* **DelayLine(L)**: fixed latency `L` chosen per mode: `0` (passthrough / DSP cancel), `~3–6 s` (streaming ML), `0` again for pre-buffered stems (already computed).
* **Mixer** does a smooth crossfade (≈ 50 ms) between original and (inst + vocal_gain·vocals) so toggling karaoke is click-free and never re-syncs.
* **Clock**: `position_ms = frames_written_to_device − device_output_latency − L`, plus the source's declared track start. This is the *single* playback clock; lyrics, video, and remote UIs all derive from it. Exposed as `(track_id, position_ms, monotonic_ts)` at 10 Hz over WS.
* **Output**: `cpal` (CoreAudio / WASAPI / ALSA-via-PipeWire). Device selectable; TV over HDMI is just another device.

**System-capture fallback** (Phase 3, "Any source" mode — for YouTube in a browser, etc.):
* Windows: WASAPI loopback (supported by `cpal`).
* macOS: ScreenCaptureKit audio capture (macOS 13+, needs Screen Recording permission), or a virtual device (BlackHole) — document both.
* Linux: PipeWire monitor node via `pipewire-rs`.
* No metadata → identify via Chromaprint fingerprint → AcoustID → MusicBrainz/ISRC. Timing is approximate (fingerprint offset).

---

## 2. Protocol & data ingestion

### 2.1 Spotify Connect — `librespot` (embedded as a Rust library)

* Implements the `Sink` trait → we receive decoded PCM directly. Implements `PlayerEvent` stream → `Playing{track_id, position_ms}`, `Paused`, `TrackChanged`, `Seeked`, `Preloading{next track_id}`, `VolumeChanged`.
* Metadata: `track_id` → `librespot_metadata::Track` gives name, artists, album, duration, **ISRC** (external_ids), cover art. No Web API needed for the core loop.
* **Pre-fetch for D5**: librespot exposes `AudioFile::open(track file_id)` — we can download + decode the *next* track (from `Preloading` event or the Connect queue) and run offline separation while the current one plays.
* Spotify Web API (user OAuth, PKCE): used only for nice-to-haves — guests adding to queue, richer search. Note: since late 2024 Spotify restricts several endpoints (audio-analysis/features, recommendations) for new apps; don't build on them.
* Caveats to state in the README: requires Spotify Premium; librespot is unofficial; the Connect device is named e.g. "🎤 Party TV".
* Runs fine on a Pi (librespot was born there).

### 2.2 AirPlay — `shairport-sync` sidecar (MVP) → native receiver on OpenAir crates (later)

Rust RAOP receivers exist but none are production-grade for AirPlay 2 timing/encryption; shairport-sync is the de-facto standard. Run as child process, fixed config, never restarted on mode switches.

* Build/bundle with `--with-metadata --with-pipe --with-airplay-2` (+ `nqptp` sidecar for AirPlay 2 PTP; AirPlay 1 needs neither and is the Windows MVP path).
* Audio: `-o pipe` → s16le 44.1k stereo → Source thread. Metadata pipe: `minm` title, `asar` artist, `asal` album, `PICT` art, `prgr` progress (RTP frames), `pbeg/pend`, `pfls` flush, `pvol`.
* Position: coarse via `prgr`; our sample counter runs between events and re-anchors.
* No ISRC → resolve by `(artist, title, album, duration)` on LRCLIB / MusicBrainz.

**Display mode (karaoke off / not installed)** — behave as a regular AirPlay 2 multiroom member. The sender (iPhone) is clock master; shairport-sync + nqptp play at PTP time. Our ring buffer + cpal add a *fixed* latency after shairport-sync's timing point: measure it (cpal device latency + ring size) and feed it back via `audio_backend_latency_offset_in_seconds` so sync error stays within a few ms. Audio tee'd for the visualizer only.

**Karaoke mode** — we intentionally play `L` late; sender-side grouping is broken by construction (no protocol flag exists to opt out of grouping, and none is needed). UI shows: *"Karaoke on — send only to Party TV; use Rebeam for other rooms."* Nothing restarts: the delay lives entirely on our side. Lookahead: AirPlay 2 senders run ≈ 2 s ahead of playback; a negative backend latency offset can surface most of that buffer to us early → streaming separation with little *additional* user-visible latency. *(To verify on real hardware in Phase 2.)*

**Long-term: native AirPlay 2 receiver.** [OpenAir](https://github.com/Antoinenz/OpenAir) already implements the AirPlay 2 stack as Rust crates (`crypto`, `pairing`, `rtsp`, `timing`, `audio-codec`, `audio-rtp`, `discovery`). A receiver is largely the mirror of the sender; building `stems-source-airplay-native` on those crates gives an in-process, Windows-friendly receiver with direct access to the sender's buffer for lookahead. Not MVP; keep OpenAir's crate split clean so this stays possible.

### 2.2b Rebeam — multi-room *after* vocal removal (`Sink` implementations)

```rust
pub trait Sink: Send {
    fn latency(&self) -> Duration;                  // reported; mixer aligns all sinks to the slowest
    fn write(&mut self, frames: &[f32]) -> Result<()>;
    fn set_volume(&mut self, v: f32);
}
```

| Sink | Backing | Use |
|---|---|---|
| `local` | cpal | TV / HDMI / local speakers |
| `airplay` | **OpenAir** crates (`client`, `discovery`, `pairing`, `audio-codec`, `audio-rtp`) — Rust, GPL-3 | Multi-room to HomePods / Apple TVs / Sonos / shairport boxes; OpenAir handles inter-receiver sync (buffered AAC, ~300 ms+ configurable) |
| `snapcast` (P3) | snapserver protocol | Linux/Pi speakers |
| `sonobus` (P3, optional) | SonoBus sidecar (C++/JUCE, GPLv3) | Laptops/phones running SonoBus as extra speakers; low priority — different use case (low-latency peer jamming) |

Topology in karaoke + rebeam: `iPhone → stems (sole receiver) → separation → { local (delayed to match), OpenAir → rooms }`. End-to-end ≈ 2 s AirPlay in + separation lag + 0.3–1 s out. Lyrics follow **our** output clock, so they are unaffected.

**Licensing consequence:** linking OpenAir's GPL-3 crates makes the shipped binary GPL-3. stems is MIT today → **relicense to GPL-3.0-or-later before external contributors arrive.** shairport-sync and yt-dlp remain process-boundary sidecars (unaffected).

### 2.3 Unified `Source` trait

```rust
pub trait Source: Send {
    fn events(&self) -> broadcast::Receiver<SourceEvent>;   // TrackStarted{TrackRef}, Position{ms}, Paused, Seeked, Ended, NextTrackHint{TrackRef}
    fn pcm(&self) -> rtrb::Consumer<f32>;                   // interleaved stereo 44.1k
    fn prefetch(&self, t: &TrackRef) -> Option<BoxFuture<AudioBuffer>>; // Some for Spotify, None for AirPlay
}
pub struct TrackRef { id: TrackId /* Spotify | Fingerprint | Local */, isrc: Option<String>, title, artists, album, duration_ms, art: Option<Url> }
```

### 2.4 Lyrics pipeline

Priority order, each result cached in SQLite keyed by `TrackRef` identity:

1. **LRCLIB** (`/api/get?artist_name&track_name&album_name&duration`) — free, open, no key, has `syncedLyrics` (line-level LRC) for a huge catalogue. Primary.
2. Local `.lrc` folder (user-provided) — overrides.
3. Community/unofficial providers behind a feature flag (Musixmatch richsync gives word-level but has licensing constraints; NetEase). Off by default in the OSS build.
4. **Word-level from ML (Phase 2b)**: we already have a clean **vocal stem**; run forced alignment (whisper.cpp / WhisperX-style CTC alignment via `ort`) of the LRC text against the vocals → word timestamps ("enhanced LRC"). This is the differentiator: karaoke-grade highlighting without a paid provider.
5. Plain (unsynced) lyrics → shown as a scrolling page with no highlight.

Line/word offset is user-adjustable per track (±ms) and persisted.

### 2.5 Music video pipeline

**Start-up time is solved by prefetching, not by the search tool.** The slow parts are search → format resolution → TLS + initial buffer; none of them should happen at track start.

* Spotify: `NextTrackHint` arrives minutes early → search, resolve, download 720p to cache during the current track. Video starts at frame 0 with the song.
* AirPlay (no hint): show art + visualizer, fetch in background, fade video in at the correct `currentTime` once the download passes the playhead. Repeat plays are instant from cache.
* Force `-f "bv[vcodec^=avc1][height<=720]+ba[acodec^=mp4a]"` → H.264/AAC MP4 plays natively in every WebView (VP9/AV1 do **not** in WKWebView).

`VideoSearch` trait, pluggable backends:

| Backend | Pros | Cons | Default? |
|---|---|---|---|
| **yt-dlp** `ytsearch5:` + `-j` (JSON, no download) | Works everywhere, one auto-updating sidecar for search *and* stream resolution | 2–5 s per search (process spawn + scraping); YouTube breaks it monthly (auto-update) | **yes** |
| YouTube Data API v3 | Official, ~100 ms, good ranking | User brings an API key; 10k units/day quota; still needs yt-dlp to resolve the stream | opt-in fast path |
| Invidious / Piped API | Fast JSON, no Python | Public instances are being blocked/killed by YouTube since 2024; self-hosting needs `inv_sig_helper` + po_token and still gets IP-blocked; stream URLs are IP-bound → must proxy through the instance | third-party/self-host only |

Ranking: duration within ±8 % of track; channel contains `VEVO` / `Official` / `- Topic`; title excludes `live|cover|lyrics|reaction|remix|karaoke`.
Alignment: video edits ≠ album masters → cross-correlate onset envelopes over the first 60 s; accept if the peak is sharp, else fall back to visualizer + album art. Manual nudge in host UI.
Legal: ToS grey area; opt-in flag, never redistribute, cache is local and LRU-bounded.

---

## 3. Local ML vocal separation

### 3.1 Model landscape (2025/26)

| Model | Quality (vocals SDR) | Runtime / format | Streamable? | Notes |
|---|---|---|---|---|
| Centre-channel cancel (L−R + side/mid tricks) | poor (works on ~40 % of pop mixes) | pure DSP, 0 latency | yes | Phase 1 placeholder; always available. |
| **Open-Unmix (UMX/UMXL)** | ok (~6–7 dB) | ONNX, small (~30 MB), LSTM | **yes** (frame-wise) | Real-time on a decent CPU. Bottom-tier quality but the true *streaming* candidate. |
| **MDX-Net (UVR family: `Kim_Vocal_2`, `UVR-MDX-NET-Voc_FT`, `Inst_HQ_x`)** | good (~9–10 dB) | **ONNX**, ~50–70 MB | chunked (≈ 4–8 s windows, overlap-add) | **Default in-process model.** Runs on `ort` with CUDA/CoreML/DirectML. ~1–3× realtime on Apple Silicon CoreML / mid GPU; ~0.3–0.8× on laptop CPU. |
| Demucs v4 (`htdemucs`, `htdemucs_ft`) | very good (~9–10 dB, best on drums/bass) | PyTorch (ONNX export is fragile) | chunked, needs ~8 s | Sidecar only. Great for **pre-buffered** mode. |
| BS-Roformer / Mel-Band Roformer | **best** (11–12 dB) | PyTorch; ONNX exports exist but heavy | chunked | Sidecar/"pro" option; GPU strongly required. |
| Spleeter | dated (~6 dB), TF1 | — | — | Skip. |

### 3.2 `Separator` trait and backends

```rust
pub trait Separator: Send + Sync {
    fn capabilities(&self) -> Caps { streaming: bool, chunk_secs: f32, min_lookahead_secs: f32, realtime_factor_estimate: f32 }
    fn separate_offline(&self, audio: &AudioBuffer) -> Result<Stems>;                 // whole track
    fn open_stream(&self) -> Result<Box<dyn StemStream>>;                             // push chunks, pull stems with fixed lag
}
```

Backends:
* `dsp` — centre cancel; always present.
* `ort` — in-process ONNX Runtime via the `ort` crate. Execution providers selected at startup: **CUDA/TensorRT** (NVIDIA), **CoreML** (Apple Silicon), **DirectML** (any Windows GPU), **CPU** (int8-quantised model variant, `intra_op_threads = cores-1`). Model files fetched on first run from a manifest with sha256s (never in git).
* `sidecar` — Python venv (PyTorch, MPS/CUDA) spoken to over local gRPC/stdio; optional, for Demucs/Roformer.
* `remote` — same gRPC over LAN to another machine running `stemsd --worker`. **This is how the Pi dev box delegates to the Windows GPU laptop.**

### 3.3 Latency strategy (the core trick)

Two modes, chosen automatically per track:

**A. Pre-buffered (Spotify path, D5):**
1. On `TrackStarted(cur)` we also get `NextTrackHint(next)` (Connect queue / `Preloading`).
2. Fetch `next` via `prefetch()` → decode → `separate_offline()` in the background (target: finish before `cur` ends; a 3.5 min track gives ≥ 3 min budget, so even 0.5× realtime CPU is fine).
3. Store stems in cache. When `next` starts, mixer plays cached stems with `L = 0`. Karaoke toggle is instant.
4. If the user skips ahead before separation finishes → fall back to mode B for that track, or DSP.

**B. Streaming (AirPlay, or cache miss):**
1. DelayLine `L = chunk + lookahead + inference_time_margin` (e.g. MDX chunk 6 s, overlap 1 s, margin 1 s → ≈ 8 s worst case; UMX ≈ 1 s).
2. Separator thread consumes the ingest ring in windows, overlap-adds stems into stem rings; mixer reads `L` behind the write head.
3. If inference falls behind (`realtime_factor < 1`) → adaptive: swap to UMX, then to DSP, with a crossfade; never glitch.
4. Lyric clock is shifted by `L` so on-screen lyrics match what's *heard*, not what the sender thinks is playing.

**Budget table — Antoine's actual machines**

| Machine | Role | MDX-Net (ort) | Demucs (sidecar) | EP | Notes |
|---|---|---|---|---|---|
| Server — **Quadro P2000** (Pascal, 5 GB, no tensor cores) | **`remote` separation worker** | 3–6× RT | 1–2× RT | CUDA (fp32 only; fp16 is slow on Pascal) | Pre-separates a song in < 1 min. NVIDIA drops Pascal in CUDA 13 → pin ORT to a CUDA 12 build. |
| Laptop — **i7-1265U / Iris Xe** | Windows path, WebView2 stage validation | 0.4–1× RT | ~0.1× | **OpenVINO** (try first on Intel iGPU) → DirectML → CPU-int8 | Pre-buffer mode only; streaming via UMX/DSP or the remote worker. |
| **MacBook Air M1** | Primary dev box: macOS audio, WKWebView stage, CoreML | 2–3× RT | ~1× (MPS) | CoreML | Fanless: fine for bursty pre-buffer, throttles on sustained streaming. |
| Raspberry Pi 5 (4 GB) | Core, servers, web UIs, tests, kiosk stage | ~0.05× | no | — | Never inference; delegate to `remote`. |

Phase-2 EP order: **CUDA + CoreML first**, OpenVINO/DirectML second. The `remote` worker is a first-class feature, not an afterthought — it's how the laptop gets real karaoke.

---

## 4. Display & remote control

### 4.1 Stage renderer (`web/stage`) — and why web, not native

**Decision: Tauri 2 window hosting a WebGL2/WebGPU canvas.** The three usual objections, addressed:

* *Buffer times* — moot by construction (D9). Audio never touches the WebView: ingest, separation, delay line, mixing, output and rebeam are Rust/cpal. The stage receives a 10 Hz clock over WS and draws. The only media it plays is `<video>` from a **local** cache file (instant). A frozen stage tab would not affect audio on any speaker.
* *Browser limitations* — real but specific: WebKitGTK on Linux is weak (→ kiosk Chromium for the stage); WKWebView lacks VP9/AV1 in `<video>` (→ force H.264); WKWebView throttles occluded windows (→ fullscreen stage is never occluded); frame jitter ≈ 1 frame (invisible for lyrics).
* *3D / text effects* — not a limitation if the work is on the GPU. WebView2 = Chromium (WebGL2 + WebGPU); WKWebView has WebGL2 and WebGPU (Safari 26+). Three.js / PixiJS v8 + custom GLSL/WGSL give SDF text with glow/warp/dissolve, per-glyph particles, 3D transitions, post-processing. Rule: per-frame work lives in shaders and instanced buffers; JS only updates uniforms.

Native alternatives considered: **wgpu + glyphon/cosmic-text** (write SDF effects, layout and an ffmpeg→texture video path yourself; 3–5× effort), **Bevy** (great shaders/particles, no SDF text effects, immature video playback), **Godot** (Theora-only video). All would still need a web layer for remote + host panel.

**Guardrails that make the choice reversible:**
1. D9: the stage is a separate window consuming only the WS protocol → can be swapped for a native wgpu/Bevy window later with zero changes elsewhere.
2. **Phase-1 de-risk gate — "stress stage" demo**: 4K, ~200 SDF glyphs animating with glow, a 720p video layer, a shader background, 60 fps, on the M1 (WKWebView) and the i7 (WebView2). Pass → commit to web for Phase 2. Fail → pivot the stage to Bevy before effects work starts.

**Implementation**
* Framework: Svelte (or Solid) for chrome; **PixiJS v8** (WebGL2/WebGPU) for the lyric layer, MSDF `BitmapText`, per-word animation via shader uniforms, not DOM reflow. Target 60 fps at 4K on integrated GPUs.
* Layers (back → front): `<video>` (muted, `object-fit: cover`) or album art + shader background → visualizer (32-band FFT from Rust at 30 Hz) → lyrics → overlays (now playing, QR, karaoke badge, guest toasts).
* Sync: WS `(track_id, position_ms, server_mono_ts)` at 10 Hz; client median-filters the offset, extrapolates with `performance.now()`, snaps on seek, slews ≤ 2 ms/frame otherwise; `video.currentTime` corrected only when drift > 80 ms.
* Runs in: Tauri fullscreen window on the chosen display, or any browser at `http://host:7373/stage` (kiosk on the Pi behind the TV).

### 4.2 Server & protocol (`stems-server`, axum)

* `GET /api/state` — full snapshot; `WS /api/ws` — broadcasts `State` diffs (JSON Patch) + `Clock` + `Spectrum` frames.
* `POST /api/karaoke {vocal_gain: 0..1}`, `POST /api/offset {lyrics_ms, video_ms}`, `POST /api/queue`, `POST /api/source/{spotify|airplay}`.
* Roles: `guest` (default, LAN) vs `host` (token shown only on host panel). Optional room PIN. Rate-limit guest toggles (party-proofing: a "vocal toggle cooldown", or host-only lock).
* Discovery: mDNS `_stems._tcp` + `stems.local`; stage shows a **QR code** to `http://<ip>:7373/`.
* Everything is loopback + LAN only; no cloud component.

### 4.3 Guest remote (`web/remote`, mobile PWA)

* Now playing (art, title, artist, progress), big **Vocals: ON/OFF** toggle with slider, lyrics follow-along (phone as second screen), "Add to queue" (Spotify only, via host's Web API token), request a song, thumbs-up.
* Host panel adds: source selection, output device, model/backend picker, latency/offset nudges, display selection, cache stats, logs.

---

## 5. Data & persistence

* `SQLite` (`sqlx`/`rusqlite`): `tracks(id, isrc, title, artists, album, duration_ms, art_path)`, `lyrics(track_id, provider, kind: plain|line|word, body, offset_ms)`, `videos(track_id, yt_id, path, offset_ms, confidence)`, `stems(track_id, model, path_vocals, path_inst, created_at)`, `settings`.
* File cache under platform cache dir; LRU by size (default 5 GB).
* Config: `stems.toml` (source enables, device names, model, ports, flags).

---

## 6. Cross-platform matrix

| Concern | macOS | Windows | Linux |
|---|---|---|---|
| Output | CoreAudio (cpal) | WASAPI (cpal) | PipeWire/ALSA (cpal) |
| Spotify | librespot | librespot | librespot |
| AirPlay | shairport-sync (brew/bundled) | shairport-sync via bundled build (needs Bonjour; ship `mdns` in-process) | shairport-sync (apt/bundled) |
| GPU EP | CoreML | CUDA / DirectML | CUDA / (ROCm via sidecar) |
| Stage WebView | WKWebView ✅ | WebView2 ✅ | WebKitGTK ⚠️ → recommend kiosk Chromium |
| Capture fallback | ScreenCaptureKit / BlackHole | WASAPI loopback | PipeWire monitor |

---

## 7. MVP roadmap (3 phases)

### Phase 1 — "It plays, it shows lyrics, it has a button" (PoC, ~4–6 weeks)
Goal: end-to-end vertical slice on the dev Pi + one laptop.
1. [x] Workspace scaffold, `stemsd` binary, config (`stems.toml`), logging, CI on Linux/Windows/macOS.
2. [x] `stems-source-spotify`: librespot discovery (zeroconf) → custom Sink → ring → cpal output. Device visible in the Spotify app. *(advertised + mDNS-verified on the Pi; listening test pending on a machine with speakers)*
3. [x] Clock + `Source`/`Sink` traits + flush signal (pause/seek drop buffered audio); ring fill and sink latency subtracted from the audible position.
3b. [x] `stems-source-file` (symphonia + rubato) for Premium-less testing; `wav:`/`null` sinks for headless verification.
4. [ ] State machine + axum WS broadcast (port 7373).
5. [ ] `web/stage`: line-level lyrics from LRCLIB, album art background, simple highlight. Runs in Chromium kiosk on the Pi/TV.
6. [ ] `stems-sep::dsp` centre-cancel + mixer crossfade wired into the pipeline; `web/remote` with one big toggle + QR code on stage. *(Mixer with crossfade exists; not yet in the hot path.)*
7. [ ] SQLite cache for tracks/lyrics.
8. [ ] **Stress-stage demo** (rendering de-risk gate, §4.1) on M1 + i7.
9. [x] Relicense repo to GPL-3.0-or-later.
**Deliberately excluded:** ML, AirPlay, video, Tauri packaging, auth.

### Phase 2 — "Real karaoke" (~6–10 weeks)
1. `stems-sep::ort` with MDX-Net ONNX; EP auto-select (CUDA/CoreML/DirectML/CPU); model download manifest.
2. Mode A pre-buffering via librespot prefetch + `NextTrackHint`; stem cache.
3. Mode B streaming DelayLine with overlap-add; adaptive fallback ladder (MDX → UMX → DSP).
4. `remote` separator backend (P2000 server as worker) — also unlocks "one GPU box serves the party".
5. AirPlay via shairport-sync sidecar: display mode with latency-offset compensation (multiroom-correct), then karaoke mode; AirPlay 1 first on Windows, AirPlay 2 + nqptp on macOS/Linux.
5b. **Rebeam**: `stems-sink-airplay` on OpenAir crates; latency-aligned multi-sink mixer.
5c. Video prefetch pipeline (yt-dlp, H.264 720p cache) — moved up from Phase 3 because prefetch is cheap once `NextTrackHint` exists.
6. Word-level lyrics by forced alignment on the vocal stem (whisper.cpp / CTC via ort).
7. Tauri 2 shell: stage fullscreen on chosen display, host panel window; installers for macOS/Windows.

### Phase 3 — "Party-proof & pretty" (ongoing)
1. Video: onset-correlation offset matching, YouTube Data API fast path, Invidious backend.
1b. Native AirPlay 2 receiver on OpenAir crates (replaces shairport-sync sidecar, esp. on Windows). Snapcast / SonoBus sinks.
2. Visualizers + themes (shader backgrounds, MSDF word animations), multi-display.
3. Guest features: queue via Web API, requests, reactions, host lock/cooldowns, room PIN.
4. System-capture "Any source" mode + fingerprint identification.
5. Optional PyTorch sidecar (Demucs / Roformer "pro quality"), auto-update, telemetry-free crash reports, Linux packaging (Flatpak/AppImage), Pi image ("stems box").

---

## 8. Risks & open questions

* **Spotify ToS / librespot**: unofficial; Premium-only; possible breakage. Mitigate: keep the Source trait clean; local-file and AirPlay sources as alternatives.
* **YouTube ToS** for videos: opt-in, no redistribution, sidecar yt-dlp updated frequently.
* **AirPlay 2 on Windows**: shairport-sync builds are awkward there; MVP targets AirPlay 1 on Windows.
* **WebKitGTK on Linux**: use kiosk Chromium for the stage until WebKitGTK GPU is reliable.
* **Model licensing**: UVR/MDX weights are MIT; Demucs MIT; some Roformer checkpoints are non-commercial — the manifest must carry licenses.
* **Pascal EOL**: P2000 needs CUDA-12-era ORT builds; document the pin.
* **Multiroom expectations**: users will group the TV with HomePods in karaoke mode and hear echo; UI must explain rebeam clearly.
* **Open**: Pi as a *supported* deployment target ("stems box") or dev-only?
