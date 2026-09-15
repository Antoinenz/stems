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
cd web/stage && pnpm install && pnpm build && cd ../..   # the TV page
cargo run -p stemsd -- spotify --name "stems"             # Spotify Connect receiver
```

Pick **stems** in the Spotify app, then open `http://<host>:7373/stage/`.
Other commands: `stemsd play <file> [--title … --artist …]`, `stemsd devices`, `stemsd config`.

### Stage view modes

`/stage/` follows the server's view mode; `/stage/?mode=karaoke` pins one screen.
Modes: `lyrics`, `karaoke` (bigger, fewer lines), `video` (music video when resolved), `canvas`
(Spotify Canvas loop when the track has one).

### API (port 7373)

- `GET /api/state` — now playing, lyrics, karaoke state, view mode
- `WS /api/ws` — state on change + clock at 10 Hz; accepts commands
- `POST /api/command` — e.g. `{"cmd":"set_karaoke","enabled":true}`,
  `{"cmd":"set_view_mode","mode":"karaoke"}`, `{"cmd":"set_lyrics_offset","offset_ms":-300}`

### Config and files

`stemsd config` prints locations and writes a default `stems.toml` (device name, sink, port,
`lyrics_offset_ms`, Spotify options). Lyrics come from LRCLIB and are cached in SQLite;
drop hand-timed `.lrc` files (plain or enhanced `<mm:ss.xx>` word tags) into
`<config dir>/lyrics/` named `<Artist> - <Title>.lrc` or `<ISRC>.lrc` to override.

## Building

Requires stable Rust (see `rust-toolchain.toml`).

```sh
# Linux only: ALSA headers
sudo apt install libasound2-dev pkg-config

cargo build --workspace
cargo run -p stemsd -- --help
```

macOS and Windows need no extra system packages.

## License

GPL-3.0-or-later. See [`LICENSE`](LICENSE).
