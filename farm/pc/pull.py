#!/usr/bin/env python3
"""The library machine's side of the stems farm (antoine-pc, Windows).

Run every few minutes (Task Scheduler, see setup.ps1). Each run:

1. asks the GPU box which tracks are ready (`farm outbox`),
2. downloads them in batches over ssh (`farm pack` → tar stream),
3. checks every file arrived whole, installs them into the library and
   only then tells the box to delete its copy (`farm ack`),
4. finishes the tracks here: lyrics, word timing, music video (low res),
   video sync, cover art (`stemsd import --resume --stages …`),
5. rewrites the catalog (catalog.csv, opens in Excel) and the browse
   folders (shortcuts by playlist, genre and artist).

    pull.py [--config I:\\stems\\farm.json] [--no-finish] [--catalog-only]

Everything lives under one folder (default: next to this file's parent):
    stems.toml  bin\\  cache\\  library\\  inbox\\  logs\\  browse\\  catalog.csv
"""

from __future__ import annotations

import argparse
import csv
import datetime as dt
import json
import logging
import os
import shlex
import shutil
import subprocess
import sys
import tarfile
import time
from pathlib import Path

log = logging.getLogger("pull")

# what the GPU box made; everything else is ours
CLOUD_STAGES = ("audio", "stems", "analysis")
CLOUD_FIELDS = ("track", "mix_file", "frames", "loudness_lufs", "info", "stems_model", "analysis")
FINISH_STAGES = "lyrics,words,video,video_align,cover"
# a finish run whose log hasn't grown for this long is stuck
STALL_S = 15 * 60


def find_ssh(configured: str | None) -> str:
    """Windows' own ssh.exe hangs when its output goes to a pipe and there
    is no console (a scheduled task, an ssh session), so prefer the one
    that comes with Git for Windows."""
    if configured:
        return configured
    for p in (
        Path(os.environ.get("ProgramFiles", r"C:\Program Files")) / "Git" / "usr" / "bin" / "ssh.exe",
        Path(os.environ.get("LOCALAPPDATA", "")) / "Programs" / "Git" / "usr" / "bin" / "ssh.exe",
    ):
        if p.is_file():
            return str(p)
    return "ssh"


class Config:
    def __init__(self, path: Path):
        raw = json.loads(path.read_text(encoding="utf-8-sig"))
        self.root = Path(raw.get("root") or path.parent)
        cloud = raw["cloud"]
        self.ssh = [
            find_ssh(raw.get("ssh")),
            "-n",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=accept-new",
            "-o",
            f"UserKnownHostsFile={(Path.home() / '.ssh' / 'known_hosts').as_posix()}",
            "-o",
            "ServerAliveInterval=30",
        ]
        if cloud.get("port"):
            self.ssh += ["-p", str(cloud["port"])]
        if cloud.get("key"):
            self.ssh += ["-i", str(cloud["key"])]
        self.ssh.append(f"{cloud['user']}@{cloud['host']}" if cloud.get("user") else cloud["host"])
        self.farm = cloud.get("farm", "farm")
        self.batch_bytes = int(raw.get("batch_mb", 1500)) * 1_000_000
        self.peer = raw.get("name", os.environ.get("COMPUTERNAME", "pc")).lower()
        self.stemsd = Path(raw.get("stemsd") or self.root / "bin" / "stemsd.exe")

    @property
    def library(self) -> Path:
        return self.root / "library"


# ---- talking to the box ---------------------------------------------------


def remote(cfg: Config, *args: str, timeout: int = 120) -> str:
    cmd = cfg.ssh + [f"{cfg.farm} {' '.join(shlex.quote(a) for a in args)}"]
    r = subprocess.run(cmd, capture_output=True, timeout=timeout, stdin=subprocess.DEVNULL)
    if r.returncode != 0:
        raise RuntimeError(f"`farm {args[0]}` failed ({r.returncode}): {r.stderr.decode(errors='replace').strip()}")
    return r.stdout.decode()


def safe_member(name: str) -> bool:
    p = Path(name)
    return not p.is_absolute() and ".." not in p.parts and p.parts[:1] in (("tracks",), ("playlists",))


def download(cfg: Config, keys: list[str], dest: Path) -> None:
    """`farm pack` piped straight into the inbox (no temp file on either side)."""
    cmd = cfg.ssh + [f"{cfg.farm} pack {' '.join(shlex.quote(k) for k in keys)}"]
    proc = subprocess.Popen(cmd, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        with tarfile.open(fileobj=proc.stdout, mode="r|") as tar:
            for m in tar:
                if not m.isfile() or not safe_member(m.name):
                    continue
                out = dest / Path(m.name)
                out.parent.mkdir(parents=True, exist_ok=True)
                src = tar.extractfile(m)
                with open(out, "wb") as f:
                    shutil.copyfileobj(src, f, 1 << 20)
    finally:
        _, err = proc.communicate(timeout=60)
    if proc.returncode != 0:
        raise RuntimeError(f"`farm pack` failed ({proc.returncode}): {err.decode(errors='replace').strip()}")


# ---- installing ------------------------------------------------------------


def verify(inbox: Path, item: dict) -> str | None:
    """None when every file is there with the size the box reported."""
    d = inbox / "tracks" / item["key"]
    for name, size in item["files"].items():
        f = d / name
        if not f.is_file():
            return f"{name} missing"
        if f.stat().st_size != size:
            return f"{name} is {f.stat().st_size} bytes, expected {size}"
    try:
        json.loads((d / "track.json").read_text(encoding="utf-8"))
    except (OSError, ValueError) as e:
        return f"track.json unreadable: {e}"
    return None


def merge_entry(ours: dict, theirs: dict) -> dict:
    """The box's audio/stems/analysis with our lyrics/video/cover progress."""
    out = dict(ours)
    for k in CLOUD_FIELDS:
        if k in theirs:
            out[k] = theirs[k]
    stages = dict(ours.get("stages") or {})
    for s in CLOUD_STAGES:
        if s in (theirs.get("stages") or {}):
            stages[s] = theirs["stages"][s]
    out["stages"] = stages
    return out


def install(cfg: Config, inbox: Path, key: str) -> None:
    src = inbox / "tracks" / key
    dst = cfg.library / "tracks" / key
    if not dst.exists():
        dst.parent.mkdir(parents=True, exist_ok=True)
        os.replace(src, dst)
        return
    # already here (imported again on the box): new audio files, merged entry
    theirs = json.loads((src / "track.json").read_text(encoding="utf-8"))
    ours = json.loads((dst / "track.json").read_text(encoding="utf-8"))
    for f in src.iterdir():
        if f.name != "track.json":
            os.replace(f, dst / f.name)
    if (dst / "instrumental.flac").exists() and (dst / "instrumental.wav").exists():
        (dst / "instrumental.wav").unlink()
    tmp = dst / "track.json.part"
    tmp.write_text(json.dumps(merge_entry(ours, theirs), indent=2), encoding="utf-8")
    os.replace(tmp, dst / "track.json")
    shutil.rmtree(src, ignore_errors=True)


def pull(cfg: Config) -> int:
    outbox = json.loads(remote(cfg, "outbox"))
    items = outbox["tracks"]
    if not items:
        log.info("nothing waiting on the box")
        return 0
    total = sum(sum(i["files"].values()) for i in items)
    log.info("%d tracks waiting (%.1f GB)", len(items), total / 1e9)
    inbox = cfg.root / "inbox"
    got = 0
    batch: list[dict] = []
    size = 0
    batches = []
    for it in items:
        s = sum(it["files"].values())
        if batch and size + s > cfg.batch_bytes:
            batches.append(batch)
            batch, size = [], 0
        batch.append(it)
        size += s
    if batch:
        batches.append(batch)

    for n, batch in enumerate(batches, 1):
        shutil.rmtree(inbox, ignore_errors=True)
        inbox.mkdir(parents=True)
        keys = [i["key"] for i in batch]
        started = time.monotonic()
        download(cfg, keys, inbox)
        mb = sum(sum(i["files"].values()) for i in batch) / 1e6
        took = time.monotonic() - started
        log.info("batch %d/%d: %d tracks, %.0f MB in %.0f s (%.1f MB/s)", n, len(batches), len(batch), mb, took, mb / max(took, 0.1))
        good = []
        for it in batch:
            problem = verify(inbox, it)
            if problem:
                log.warning("%s (%s): %s; left on the box", it["title"], it["key"], problem)
                continue
            install(cfg, inbox, it["key"])
            good.append(it["key"])
        # playlists come with every batch; the box's copy is the latest
        pl = inbox / "playlists"
        if pl.is_dir():
            (cfg.library / "playlists").mkdir(parents=True, exist_ok=True)
            for p in pl.glob("*.json"):
                os.replace(p, cfg.library / "playlists" / p.name)
        if good:
            remote(cfg, "ack", *good)
        got += len(good)
    shutil.rmtree(inbox, ignore_errors=True)
    log.info("installed %d tracks", got)
    return got


# ---- finishing -------------------------------------------------------------


def stemsd_env(cfg: Config) -> dict:
    env = dict(os.environ)
    env["STEMS_HOME"] = str(cfg.root)
    env["NO_COLOR"] = "1"
    bin_dir = str(cfg.root / "bin")
    env["PATH"] = bin_dir + os.pathsep + env.get("PATH", "")
    dll = cfg.root / "bin" / "onnxruntime.dll"
    if dll.is_file():
        env["ORT_DYLIB_PATH"] = str(dll)
    return env


def finish_needed(cfg: Config) -> bool:
    """Pending work, or failures not retried for a few hours (a video that
    can't be found shouldn't be searched for every ten minutes)."""
    stamp = cfg.root / "logs" / "last-retry"
    retry_failed = not stamp.exists() or time.time() - stamp.stat().st_mtime > 4 * 3600
    tracks, _ = load_library(cfg.library)
    states = {stage(e, s) for e in tracks.values() for s in FINISH_STAGES.split(",")}
    if "pending" in states:
        return True
    if "failed" in states and retry_failed:
        stamp.write_text(dt.datetime.now().isoformat())
        return True
    return False


def finish(cfg: Config) -> None:
    if not finish_needed(cfg):
        return
    if not cfg.stemsd.is_file():
        log.warning("no stemsd at %s; skipping lyrics/video", cfg.stemsd)
        return
    log.info("finishing tracks: %s", FINISH_STAGES)
    logfile = cfg.root / "logs" / f"stemsd-{dt.date.today():%Y-%m-%d}.log"
    with open(logfile, "ab") as out:
        proc = subprocess.Popen(
            [str(cfg.stemsd), "import", "--resume", "--stages", FINISH_STAGES],
            env=stemsd_env(cfg),
            stdout=out,
            stderr=subprocess.STDOUT,
            creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
        )
        # watchdog: a run whose log stops growing is stuck (a hung download);
        # kill it — every track's progress is saved, the next run goes on
        last_size, last_change = -1, time.monotonic()
        while proc.poll() is None:
            time.sleep(30)
            size = logfile.stat().st_size
            if size != last_size:
                last_size, last_change = size, time.monotonic()
            elif time.monotonic() - last_change > STALL_S:
                log.warning("stemsd made no progress for %d min; stopping it", STALL_S // 60)
                proc.kill()
                for name in ("yt-dlp.exe", "deno.exe"):
                    subprocess.run(["taskkill", "/F", "/T", "/IM", name], capture_output=True)
                break
        proc.wait()
        r = proc
    log.info("stemsd finished with %d (log: %s)", r.returncode, logfile.name)


# ---- catalog and browse folders --------------------------------------------


def load_library(lib: Path):
    tracks = {}
    for d in sorted((lib / "tracks").glob("*")):
        try:
            tracks[d.name] = json.loads((d / "track.json").read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
    playlists = []
    for p in sorted((lib / "playlists").glob("*.json")):
        try:
            playlists.append(json.loads(p.read_text(encoding="utf-8")))
        except (OSError, ValueError):
            continue
    playlists.sort(key=lambda p: p.get("name", "").lower())
    return tracks, playlists


def stage(e: dict, name: str) -> str:
    return (e.get("stages") or {}).get(name, {}).get("state", "pending")


def pct(x) -> str:
    return "" if x is None else str(round(100 * x))


def spotify_url(e: dict) -> str:
    tid = e["track"].get("id") or {}
    return f"https://open.spotify.com/track/{tid['id']}" if tid.get("kind") == "spotify" else ""


def main_genre(e: dict) -> str:
    g = ((e.get("analysis") or {}).get("genres") or [{}])[0].get("name", "")
    return g.split(" / ")[0]


def update_index(cfg: Config) -> None:
    """library.sqlite: incremental, one transaction, safe to run alongside
    the other task (SQLite makes concurrent updates take turns)."""
    if not cfg.stemsd.is_file():
        return
    r = subprocess.run(
        [str(cfg.stemsd), "index"],
        env=stemsd_env(cfg),
        capture_output=True,
        text=True,
        creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
    )
    if r.returncode == 0:
        log.info("index: %s", r.stdout.strip().split(": ", 1)[-1])
    else:
        log.warning("index update failed: %s", (r.stderr or r.stdout).strip()[-300:])


def write_catalog(cfg: Config) -> None:
    tracks, playlists = load_library(cfg.library)
    in_lists: dict[str, list[str]] = {}
    for p in playlists:
        for k in p.get("tracks", []):
            in_lists.setdefault(k, []).append(p["name"])
    rows = []
    for key, e in tracks.items():
        t, a, info = e["track"], e.get("analysis") or {}, e.get("info") or {}
        moods = a.get("moods") or {}
        genres = a.get("genres") or []
        lyrics = stage(e, "lyrics")
        rows.append(
            {
                "Title": t["title"],
                "Artist": ", ".join(t.get("artists") or []),
                "Album": t.get("album") or "",
                "Year": (info.get("release_date") or "")[:4],
                "Length": f"{t.get('duration_ms', 0) // 60000}:{t.get('duration_ms', 0) // 1000 % 60:02d}",
                "BPM": a.get("bpm", ""),
                "Key": a.get("key", ""),
                "Danceability %": pct(a.get("danceability")),
                "Party %": pct(moods.get("party")),
                "Happy %": pct(moods.get("happy")),
                "Sad %": pct(moods.get("sad")),
                "Relaxed %": pct(moods.get("relaxed")),
                "Aggressive %": pct(moods.get("aggressive")),
                "Genre": main_genre(e),
                "Style": genres[0]["name"].split(" / ")[-1] if genres else "",
                "Other styles": "; ".join(g["name"] for g in genres[1:3]),
                "Popularity": info.get("popularity", ""),
                "Explicit": {True: "yes", False: "no"}.get(info.get("explicit"), ""),
                "Playlists": "; ".join(in_lists.get(key, [])),
                "Karaoke ready": "yes" if stage(e, "audio") == "done" and stage(e, "stems") == "done" else "no",
                "Lyrics": {"done": "word-timed" if stage(e, "words") == "done" else "lines", "missing": "none"}.get(lyrics, lyrics),
                "Video": {"done": "yes", "missing": "none"}.get(stage(e, "video"), stage(e, "video")),
                "Spotify": spotify_url(e),
                "Stems model": e.get("stems_model") or "",
                "Folder": key,
            }
        )
    rows.sort(key=lambda r: (r["Artist"].lower(), r["Title"].lower()))
    out = cfg.root / "catalog.csv"
    tmp = out.with_suffix(".csv.part")
    with open(tmp, "w", newline="", encoding="utf-8-sig") as f:  # BOM: Excel reads it as UTF-8
        w = csv.DictWriter(f, fieldnames=list(rows[0].keys()) if rows else ["Title"])
        w.writeheader()
        w.writerows(rows)
    try:
        os.replace(tmp, out)
    except PermissionError:
        log.warning("catalog.csv is open in Excel; wrote catalog-new.csv instead")
        os.replace(tmp, cfg.root / "catalog-new.csv")
    log.info("catalog: %d tracks, %d playlists", len(rows), len(playlists))
    write_browse(cfg, tracks, playlists)


BAD = '<>:"/\\|?*'


def clean(name: str, limit: int = 80) -> str:
    s = "".join("_" if c in BAD or ord(c) < 32 else c for c in name).strip().rstrip(".")
    return (s[:limit].rstrip() or "_")


def shortcut(path: Path, target: Path) -> None:
    """A Windows Internet Shortcut to the track folder: a tiny text file,
    so copying the library never duplicates audio (unlike junctions)."""
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(f"[InternetShortcut]\r\nURL={target.as_uri()}\r\n", encoding="utf-8")


def write_browse(cfg: Config, tracks: dict, playlists: list) -> None:
    root = cfg.root / "browse"
    tmp = cfg.root / "browse.part"
    shutil.rmtree(tmp, ignore_errors=True)
    tmp.mkdir(parents=True)
    lib = cfg.library / "tracks"

    def label(e: dict) -> str:
        return clean(f"{', '.join(e['track'].get('artists') or [])} - {e['track']['title']}")

    for p in playlists:
        folder = tmp / "Playlists" / clean(p["name"])
        for n, key in enumerate(p.get("tracks", []), 1):
            if key in tracks:
                shortcut(folder / f"{n:03d} {label(tracks[key])}.url", lib / key)
    for key, e in tracks.items():
        genre = main_genre(e) or "Unknown"
        shortcut(tmp / "Genres" / clean(genre) / f"{label(e)}.url", lib / key)
        artist = (e["track"].get("artists") or ["Unknown"])[0]
        shortcut(tmp / "Artists" / clean(artist) / f"{clean(e['track']['title'])}.url", lib / key)
    (tmp / "README.txt").write_text(
        "Shortcuts to the track folders in ..\\library\\tracks, by playlist, genre and artist.\r\n"
        "Rebuilt on every sync: don't put anything here. Sort and filter in ..\\catalog.csv.\r\n",
        encoding="utf-8",
    )
    shutil.rmtree(root, ignore_errors=True)
    os.replace(tmp, root)


# ---- main -------------------------------------------------------------------


def main() -> None:
    here = Path(__file__).resolve().parent
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--config", type=Path, default=here.parent / "farm.json")
    ap.add_argument("--no-finish", action="store_true", help="only download, no lyrics/video")
    ap.add_argument("--finish-only", action="store_true", help="only lyrics/video/cover and the catalog, no download")
    ap.add_argument("--catalog-only", action="store_true")
    args = ap.parse_args()
    cfg = Config(args.config)
    (cfg.root / "logs").mkdir(parents=True, exist_ok=True)
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s %(levelname)s %(message)s",
        handlers=[
            logging.FileHandler(cfg.root / "logs" / f"pull-{dt.date.today():%Y-%m-%d}.log", encoding="utf-8"),
            *([logging.StreamHandler(sys.stdout)] if sys.stdout else []),
        ],
    )
    # downloading and finishing run as separate tasks with separate locks,
    # so a long stretch of video downloads never holds up collecting songs
    # from the box (which is rented by the hour)
    mode = "finish" if (args.finish_only or args.catalog_only) else "download" if args.no_finish else "sync"
    lock = cfg.root / "logs" / f"{mode}.lock"
    if lock.exists() and time.time() - lock.stat().st_mtime < 6 * 3600:
        log.info("another %s run is in progress (%s)", mode, lock)
        return
    lock.write_text(str(os.getpid()))
    try:
        if not (args.catalog_only or args.finish_only):
            try:
                # keep going while the box has more (a backlog drains in one run)
                while pull(cfg) and args.no_finish:
                    update_index(cfg)
            except Exception:  # noqa: BLE001 - finish what we have anyway
                log.exception("pulling from the box failed")
            update_index(cfg)
        if not (args.no_finish or args.catalog_only):
            finish(cfg)
            update_index(cfg)
        if not args.no_finish:
            write_catalog(cfg)
    finally:
        lock.unlink(missing_ok=True)

if __name__ == "__main__":
    main()
