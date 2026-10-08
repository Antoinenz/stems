# Mac app redesign

The Mac sits on a dining table with the audio gear at an outdoor party. Whoever
uses it is standing in front of it: regular macOS sizes. From across the space
the phone app is the remote, with the same controls. Appearance follows the
system (dark in practice).

## Main window: the show

One view for running the night, not a page among many:

- **Now playing** (left/top): artwork, title, artist, whose turn it is,
  progress, play/pause, skip, karaoke on/off, guide vocals, volume, the current
  and next lyric line, the stage's view mode.
- **Queue** (right): requests in order with singer names and sing/play badges;
  drag to reorder, from the library too; "Up next: Mia — get ready".
- **Add songs**: a search field over the library (and Spotify, below) that
  queues without leaving the view.
- **Mode switch** (toolbar): Background · Party · Karaoke (see Modes).
- **End-of-song controls** (toolbar + menu + phone), for announcements:
  - *Hold after this song*: let it finish, then fade to silence and wait.
  - *Background after this song*: let it finish, then quiet background music
    (well under the music level, easy to talk over) until resumed.
  - *Fade out now*: fade to silence over a few seconds, then hold.
  - *Talk-over (hold)*: duck to ~−18 dB while held, back up on release.
  - *Resume*: start the next song.

## Library

Sidebar:

- **Playlists**, organised in **folders** the host makes (imported Spotify
  playlists are read-only lists, but can be filed into folders; local
  playlists are editable).
- **Songs**, **Artists**, **Genres**, **Smart lists** (Dance floor, Karaoke
  ready, Slow ones, by decade, Recently played, Never played).

Songs table (sortable, column picker): title, artist, album, year, length,
BPM, key, danceability, genre/style, plays, last played, lyrics (lines/words),
video, stems model.

**Song page** (opens on click, or in an inspector): large cover, analysis
(danceability, moods, top styles), lyrics, video thumbnail and sync offset,
which playlists it's in, play history, import status with retry, actions:
Sing / Play / Play next / Add to playlist.

**Preview on the Mac's own speakers** (or headphones), never the party
output: an AVAudioEngine player on a chosen local device, with Mix /
Instrumental / Vocals.

**Search**: the local database first (titles, artists, lyrics), then — if
Spotify search is on — matches from Spotify with a **Download** button that
imports them (separation on the worker, or locally). A switch turns Spotify
off entirely when the venue's internet is poor.

## Modes

| | Background | Party | Karaoke |
|---|---|---|---|
| Plays | chosen playlists / moods | a playlist | requests |
| When the queue is empty | auto-DJ | auto-DJ | auto-DJ (toggle) or quiet background |
| Guest requests | off | play-only, queued | sing (and play), encouraged |
| Karaoke | off | off | on for sung requests |
| Level | lower | normal | normal |

**Auto-DJ**: picks the next song from the allowed pool (mode playlists,
smart lists) by similarity to what just played — genre/style scores, moods,
BPM (close, or ×2/÷2), key compatibility, danceability, era — minus anything
played recently (play history), with a little randomness. Energy can follow a
curve (build up through the night).

**Karaoke flow**: a sung request doesn't cut in. The host gets a phone
notification; the request waits as *staged*. When the current song ends the
player holds (silence or quiet background); the host announces the singer, the
singer sets up the screen, the host presses **Start**. Optionally the
next-song countdown shows on the stage ("Mia is up next").

Phone notifications: a free Apple ID can't use Apple push, so the server
sends them through **ntfy** (free app; works over mobile data), and the iOS
app shows them in-app when open.

## Machines

**Status** grouped per machine — never a bare "server load":

- **This Mac** (the server): CPU, memory, temperature, thermal state, audio
  output and dropouts, the stemsd process.
- **Workers** (SL7, a rented GPU…): online, address, provider (DirectML,
  CUDA…), speed, queue, errors, their own CPU/GPU/temperature (reported by
  the worker).
- **Screens** (stage on the SL5): connected, view mode, frame rate, video
  buffered, drift.
- **Phones / remotes**: host and guest counts, names.

**Manage machines** panel:

- list with name, role (separation worker, lyrics screen, library sync,
  remote), connection (address, port, dial in / listen, status), priority
  (which worker first, fallback order), enabled;
- **Add machine**: shows exactly what to run on it (worker zip + the Mac's
  address, or a QR for the stage/phone) and waits for it to appear;
- edit, disable, forget.

Backed by a machine registry in `stems.toml` (`[[machines]]`) merged with
live status; workers and screens report their own system stats.

## Backend this needs

1. Library query API on `library.sqlite`: search (FTS), sort, filter, page;
   playlists (+ folders), artists, genres, song detail, cover thumbnails.
2. Play history (append-only file in the library, indexed).
3. Playback control: hold/fade/background-after-song, talk-over duck, modes,
   staged karaoke requests, auto-DJ.
4. Spotify search + import from the app (with an off switch).
5. Machine registry, per-machine stats from workers and screens.
6. Notifications (ntfy).

## Order

1. Query API + play history → new window shell, show view (now playing +
   queue + add), songs table, follow system appearance.
2. End-of-song controls and modes (background / party / karaoke flow) on the
   server, in the Mac and phone apps.
3. Song page, playlists with folders, smart lists, Mac-speaker preview.
4. Machines: per-machine status and the manage panel.
5. Auto-DJ, Spotify search/download, notifications.
