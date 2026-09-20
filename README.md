# stems

**Open-source party karaoke box.** Runs on a desktop (or a small box), projects to a TV, acts as a
Spotify Connect / AirPlay receiver, renders synced lyrics and music videos, and does local ML vocal
removal — so any song becomes a karaoke track.

> Status: **pre-alpha**. Nothing usable yet. Follow the roadmap in
> [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) §7.

## How it works (short version)

- The app **is the audio sink**: your phone streams to it via Spotify Connect (librespot) or
  AirPlay (shairport-sync), so it gets raw PCM plus exact metadata and timing.
- A Rust core (`stemsd`) owns the audio path: ingest → optional vocal separation → delay line →
  mixer → local output and/or multi-room rebeam.
- Stage display, host panel and guest remote are web clients of the same local server
  (default port `7373`). Phones on the LAN scan a QR code to toggle vocals and see lyrics.
- Separation runs in-process (ONNX) or on another machine with a GPU on your LAN.

Full design: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Running (pre-alpha)

```sh
cd web/stage && pnpm install && pnpm build && cd ../..    # the TV page
cd web/remote && pnpm install && pnpm build && cd ../..   # the phone remote
cargo run -p stemsd -- spotify --name "stems"             # Spotify Connect receiver
```

When run from a checkout, stemsd rebuilds `web/stage` and `web/remote` at start if their
sources are newer than `dist/` (needs `pnpm`; set `STEMS_NO_WEB_BUILD=1` to skip).
Pick **stems** in the Spotify app, then open `http://<host>:7373/stage/` on the TV. Guests
control it from `http://<host>:7373/remote/` on their phones — the stage shows a QR code for
it while idle and a small one in the corner while playing (`/stage/?qr=0` hides it). The
remote has the karaoke toggle, vocal level, volume, stage mode, and lyric/video timing nudges.
Other commands: `stemsd play <file> [--title … --artist …]`, `stemsd devices`, `stemsd config`,
`stemsd worker`.

### Stage view modes

`/stage/` follows the server's view mode; `/stage/?mode=karaoke` pins one screen.
Modes: `lyrics`, `karaoke` (bigger, fewer lines), `video` (music video when resolved), `canvas`
(Spotify Canvas loop when the track has one).

### API (port 7373)

- `GET /api/state` — now playing, lyrics, karaoke state, view mode
- `GET /api/info` — LAN URLs of the stage and remote; `GET /api/remote-qr.svg` — QR of the latter
- `GET /api/system` — CPU %, memory and temperature sensors of the box (shown on the remote)
- `WS /api/ws` — state on change + clock at 10 Hz; accepts commands
- `POST /api/command` — e.g. `{"cmd":"set_karaoke","enabled":true}`,
  `{"cmd":"set_view_mode","mode":"karaoke"}`, `{"cmd":"set_lyrics_offset","offset_ms":-300}`,
  `{"cmd":"set_video_offset","offset_ms":1500}`

### Config and files

`stemsd config` prints locations and writes a default `stems.toml` (device name, sink, port,
`lyrics_offset_ms`, Spotify options). Lyrics come from LRCLIB and are cached in SQLite;
drop hand-timed `.lrc` files (plain or enhanced `<mm:ss.xx>` word tags) into
`<config dir>/lyrics/` named `<Artist> - <Title>.lrc` or `<ISRC>.lrc` to override.

### Music videos

```toml
[video]
enabled = true          # off by default
ytdlp = "yt-dlp"        # executable name or path; ffmpeg must be on PATH too
max_height = 1080
```

With this on, every track triggers a `yt-dlp` search (`<artist> - <title> official video`) and a
one-time download into `<cache dir>/video/`; when it lands the stage's `video` mode plays it
from `/media/video/…` on the track's timeline. Videos and streams rarely share a timeline
(intros, edits): `set_video_offset` shifts the video in ms until automatic alignment exists.

### Karaoke backends

`[karaoke]` in `stems.toml`:

```toml
[karaoke]
backend = "mdx"                    # "dsp" (instant centre-cancel) or "mdx" (ONNX model)
model = "UVR-MDX-NET-Inst_HQ_3"    # or "Kim_Vocal_2"; downloaded on first use
device = "auto"                    # auto | cpu | cuda | directml | coreml (GPU) | coreml-all | coreml-ane
threads = 0                        # CPU threads, 0 = runtime default
slack_ms = 2000                    # extra latency budget for slow inference bursts
hop_ms = 0                         # new audio per inference; 0 = full window (~5.9 s)
lookahead_ms = 0                   # future context before output; 0 = model minimum
denoise = false                    # run x and -x, average: fewer artefacts, 2x cost
workers = 1                        # parallel model sessions; 2 keeps a GPU busy
fade_ms = 400                      # crossfade when karaoke is toggled
```

Latency ≈ `lookahead + hop + slack`. Smaller `hop_ms` blends overlapping windows (better
quality, lower latency) at `5900 / hop_ms` times the inference cost, so it is bounded by how
fast the device runs one window. Starting points: GPU with headroom → `hop_ms = 2000,
lookahead_ms = 1500, slack_ms = 1500` (~5 s); fast GPU → `hop_ms = 1200, lookahead_ms = 1000,
slack_ms = 1200` (~3.4 s). If the log says `mdx separator starved`, raise `hop_ms`/`slack_ms`.

On Apple Silicon `coreml` runs the model on the GPU (~0.45 s per window on an M1 Air).
`coreml-ane` targets the Neural Engine (lower power, so a fanless machine throttles less) and
`coreml-all` lets Core ML choose. The model's batch dimension is fixed at load, which Core ML
needs to compile for the Neural Engine at all.

Each window is a serial chain — STFT on the CPU, the model on the GPU, iSTFT on the CPU —
so with one worker the GPU idles between windows and shows up as spikes. `workers = 2` runs
two windows concurrently (second model session, ~2× GPU memory) so the CPU legs overlap the
GPU work; go to 3 on a discrete GPU with room. When inference slows down (thermal throttling on a fanless
laptop, a background job on the GPU) the separator thins the overlap — it skips windows
that neighbours already cover, so quality dips slightly instead of the audio dropping out
(`mdx load high: thinning windows` / `mdx load normal` in the log). Only when even that is
not enough are windows dropped (`dropping windows`): those seconds pass through dry rather
than the delay growing without bound.

`mdx` streams audio through the model in ~6 s windows, so playback runs ~8 s behind the
controller (lyrics and clock account for it). Inference must be at least real time: fine on a
GPU or Apple Silicon, marginal on laptop CPUs, not viable on a Raspberry Pi. GPU providers are
compile-time features: `cargo build -p stemsd --features cuda` (NVIDIA, CUDA 12 runtime
required), `--features directml` (any Windows GPU), `--features coreml` (macOS).

### Pre-buffer mode (zero latency)

```toml
[karaoke]
mode = "prebuffer"      # default: "streaming"
```

Instead of separating the live stream a few seconds behind the phone, `prebuffer` downloads
each track as it starts (and the *next* track as soon as Spotify announces it), runs the model
over the whole song as fast as the device allows, and plays with **no added delay** from
stems computed ahead of the playhead. On an M1 Air a song is done in ~10–20 s; until the job
overtakes the playhead the vocals simply stay in (the remote shows `preparing 37%` → `stems
ready`). Tracks that were announced ahead of time are fully ready before they start. Skips
and seeks are instant. The streaming mode remains for machines that cannot separate faster
than real time.

### Remote worker

The model can run on another machine — a desktop GPU serving a Raspberry Pi or a laptop:

```sh
# on the GPU box (uses its own stems.toml [karaoke] model/device/workers)
cargo run -p stemsd --features cuda -- worker --bind 0.0.0.0:7374
```

```toml
# on the player
[karaoke]
backend = "remote"
remote = "192.168.1.50:7374"
hop_ms = 1000
lookahead_ms = 1000
slack_ms = 1500
denoise = true
workers = 2          # parallel connections; the worker's own `workers` bounds concurrency
```

Windows (~2 MB each way) stream over TCP; the worker answers with the instrumental and the
player blends them exactly as it would locally, so latency and quality knobs are the same.
`denoise` is applied on the worker. Several players can share one worker.

## Building

Requires stable Rust (see `rust-toolchain.toml`).

```sh
# Linux only: ALSA headers
sudo apt install libasound2-dev pkg-config

cargo build --workspace
cargo run -p stemsd -- --help
```

macOS and Windows need no extra system packages. The default build downloads a prebuilt
ONNX Runtime; where that does not link (e.g. Debian 12 on aarch64) use
`--no-default-features --features onnx-dylib` and point `ORT_DYLIB_PATH` at a
`libonnxruntime.so` from the official releases, or `--no-default-features` for a DSP-only
build.

## License

GPL-3.0-or-later. See [`LICENSE`](LICENSE).
