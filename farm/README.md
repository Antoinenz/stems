# stems farm

Importing a big party set with the best vocal removal, split across two
machines:

- **The GPU box** (a rented Linux server with an NVIDIA GPU) downloads the
  songs from Spotify, separates them with a Roformer model (much cleaner than
  what a laptop can run) and analyses them: tempo, key, danceability, moods,
  genres. Only that: it never stores more than it has to, and deletes each
  song once the library machine has it.
- **The library machine** (here antoine-pc, Windows, the Backup drive `I:`)
  pulls the finished songs every few minutes over ssh, then adds lyrics, word
  timing, a low-res music video, video sync and cover art, and keeps a catalog.

```
  Spotify ──► GPU box: stemsd import --stages audio,stems,analysis
                │  (spool) ⇄ separator.py: Roformer + Essentia
                ▼
             outbox: mix.ogg + instrumental.flac + track.json   (~30 MB/song)
                │   pulled by the library machine (ssh, tar stream, size-checked)
                ▼
  I:\stems\library\tracks\<key>\  ──► stemsd import --resume --stages lyrics,words,video,video_align,cover
  I:\stems\catalog.csv, browse\   (by playlist, genre, artist)
```

The library is the normal stems library format, so it can be copied to the
party drive as is and played by `stemsd library` / the Mac app.

## The GPU box

```sh
curl -fsSL https://raw.githubusercontent.com/Antoinenz/stems/main/farm/cloud/setup.sh | bash
scp -r ~/Library/Caches/stems/spotify/credentials.json <box>:~/stems-farm/home/cache/spotify/
# the library machine's public key (printed by its setup) into ~/.ssh/authorized_keys
farm add https://open.spotify.com/playlist/…   # any number of links
farm status
```

`farm status` shows what's running, queued, ready, sent and failed, plus the
GPU. `farm logs separator` / `farm logs runner` follow the logs. Fetching
pauses when 1200 songs are waiting to be sent or the disk has less than 8 GB
free (`STEMS_FARM_MAX_OUTBOX`, `STEMS_FARM_MIN_FREE_GB`). The separation model
is `model_bs_roformer_ep_317_sdr_12.9755.ckpt` unless `STEMS_FARM_MODEL` says
otherwise (`audio-separator --list_models` lists them; each track records
which model made it).

Files: `~/stems-farm/{bin,venv,repo,models,home,queue,logs}`; `home/` is the
`STEMS_HOME` with `stems.toml` (`backend = "spool"`) and the library.

## The library machine (Windows)

```powershell
powershell -ExecutionPolicy Bypass -File farm\pc\setup.ps1 -Root I:\stems
```

It downloads yt-dlp and Deno into `I:\stems\bin`, writes `stems.toml`
(videos at most 480p) and `farm.json`, makes an ssh key, and schedules
`farm\pull.py` every 10 minutes ("stems farm sync"). Put `stemsd.exe` and its
DLLs in `I:\stems\bin` (`scripts/package-windows-worker.sh x64` builds them)
and the word-alignment model in `I:\stems\cache\models`, then fill in the
box's address in `farm.json`:

```json
{ "cloud": { "host": "1.2.3.4", "port": 22, "user": "root",
             "key": "C:\\Users\\me\\.ssh\\stems_farm", "farm": "farm" },
  "name": "antoine-pc", "batch_mb": 1500 }
```

`python I:\stems\farm\pull.py` runs one sync by hand (`--no-finish` only
downloads, `--catalog-only` just rewrites the catalog). It uses Git for
Windows' `ssh.exe` when installed: Windows' own one hangs when its output goes
to a pipe without a console, as in a scheduled task.

```
I:\stems\
  library\tracks\<key>\   track.json, mix.ogg, instrumental.flac, lyrics.json, video.mp4, cover.jpg
  library\playlists\      one JSON per imported playlist/album (name, link, tracks in order)
  library\library.sqlite  the searchable index of all of it (see below)
  catalog.csv             every song: artist, title, year, BPM, key, danceability, moods,
                          genre, playlists, lyrics/video status, Spotify link (opens in Excel)
  browse\                 shortcuts to the track folders by playlist, genre and artist
  logs\  bin\  cache\  stems.toml  farm.json  farm\
```

## The library index

`library\library.sqlite` is a SQLite database built from the track folders
(`stemsd index`; the sync tasks run it after every change, in about 0.3 s).
Tables: `tracks` (title, artists, year, BPM, key, danceability, main genre,
playable/lyrics/video flags…), `artists` + `track_artists`, `playlists` +
`playlist_tracks` (in order, including songs that never imported), `genres`
(top styles with scores), `moods`, `stages` (import progress and errors), a
`playlist_songs` view, and `search`, a full-text index over titles, artists,
albums and lyrics. Open it with any SQLite tool (e.g. DB Browser for SQLite):

```sql
SELECT artist_line, title, danceability FROM tracks
WHERE playable AND year BETWEEN 1980 AND 1989 ORDER BY danceability DESC;

SELECT t.title FROM search s JOIN tracks t USING (key)
WHERE search MATCH 'lyrics:"dancing queen"';
```

The folders stay the source of truth: the index only reads them, updates in
one transaction (readers never see a half-updated library; two updates at once
take turns) and rebuilds itself from scratch if deleted or if its schema
changes.

## The spool

`[karaoke] backend = "spool"` makes `stemsd import` hand each track to an
external daemon through a folder instead of separating it itself: the decoded
mix goes in as a float WAV, the instrumental comes back as FLAC of exactly the
same length, and an analysis as JSON. See `crates/stemsd/src/spool.rs` and
`farm/cloud/separator.py`. `separator.py --dummy` tests the plumbing without
models.
