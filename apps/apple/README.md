# Stems for Mac and iPhone

Native apps for running a party with stems:

- **Stems (Mac)** is the hub. It runs `stemsd` for you (starts it, keeps its log, restarts
  it if it crashes, keeps the Mac awake and out of App Nap) and shows the party: now
  playing, queue, library and imports, machine health, guest permissions. It stays in the
  menu bar; closing the window doesn't stop the music, quitting asks first.
- **Stems (iPhone/iPad)** is a remote for the host. Pair it once by scanning the code the
  Mac shows, then run everything from the phone.

The web remote (`/remote/`) still does everything critical, so the party never depends on
these apps.

## Build and run

Needs Xcode 26 or later and [XcodeGen](https://github.com/yonaskolb/XcodeGen)
(`brew install xcodegen`). The Xcode project is generated, not committed:

```sh
cd apps/apple
xcodegen            # writes Stems.xcodeproj
open Stems.xcodeproj
```

- **Mac:** run the `StemsMac` scheme. The build also builds `stemsd` (release, CoreML) and
  the web apps and copies them into `Stems.app`, so the app works on its own. The first
  build takes a few minutes for that (set `STEMS_SKIP_EMBED=1` to skip it; the app then
  uses `target/release/stemsd` from this checkout). It signs to run locally; no Apple
  developer account needed.
- **iPhone:** copy `Local.xcconfig.example` to `Local.xcconfig`, put in your team ID (a
  free Apple ID works) and a bundle prefix of your own, then run the `StemsiOS` scheme on
  your phone. Apps signed with a free Apple ID expire after 7 days: reinstall it shortly
  before the party.

From the command line:

```sh
xcodebuild -project Stems.xcodeproj -scheme StemsMac build
xcodebuild -project Stems.xcodeproj -scheme StemsiOS -destination 'generic/platform=iOS Simulator' build
```

## Pairing

On the Mac, choose **Stems › Pair iPhone…** and click **Show Code**. On the phone, open
Stems and tap **Scan Code** (or point the Camera app at it: the code is a
`stems://pair?…` link that opens the app). The code holds the Mac's LAN address, its
`.local` name and the host token, so whoever scans it controls the party. Typing an
address instead connects as a guest.

## Mac app settings

**Settings › General:** play from the library (offline) or Spotify Connect, the menu bar
item, which `stemsd` to run, and which `stems.toml` to use, e.g. one kept on the library
drive. **Settings › Party:** the server's live settings (karaoke for sung requests,
background music and its level, loudness target, music videos).

If a `stemsd` is already running on the configured port (started from a terminal), the app
uses it instead of starting another, and takes over if it stops.

Playback has no keyboard shortcuts on purpose (a bumped key or a space typed in the wrong
place must never stop the music). ⌘1…⌘4 switch screens, ⌥⌘L opens the server log.

## Code

- `StemsKit/`: Swift package. `StemsKit` has the API models, the client (WebSocket with
  reconnect and a dead-connection watchdog, REST for commands), the playback clock and
  pairing links. `StemsUI` has the screens both apps share.
- `Mac/`, `iOS/`: the two apps.
- `StemsKit/Tests/StemsKitTests/Fixtures/`: JSON of every API payload, written by the
  Rust side (`cargo test -p stemsd fixtures`; `STEMS_UPDATE_FIXTURES=1` to rewrite). The
  Swift tests decode all of them, so the app and the server can't drift apart unnoticed:

  ```sh
  cd StemsKit && swift test
  # against a running server too:
  STEMS_LIVE=127.0.0.1:7373 STEMS_LIVE_TOKEN=<host token> swift test --filter Live
  ```

## AV1 music videos

Macs before M3 have no AV1 decoder, and AVFoundation can't play AV1 on
them. The song preview window decodes those videos with dav1d (VideoLAN,
BSD-2-Clause; `Vendor/dav1d/COPYING`), built as a universal static library
by `scripts/build-dav1d.sh` (needs `brew install meson ninja nasm`). The
built library is committed so a normal build doesn't need those tools.
