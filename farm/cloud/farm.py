#!/usr/bin/env python3
"""The GPU box's side of the stems farm.

    farm add <spotify links…>   queue playlists/albums/tracks (starts everything)
    farm start | stop           the separator daemon and the import runner
    farm status [--json]        what's running, queued, done, sent, failed
    farm logs [separator|runner]
    # used by antoine-pc over ssh:
    farm outbox                 tracks ready to send, with file sizes (JSON)
    farm pack <keys…>           tar of those track folders + playlists, to stdout
    farm ack <keys…>            they arrived: delete the media, keep track.json

Layout under $FARM_HOME (default ~/stems-farm):
    bin/stemsd  venv/  repo/  models/
    home/            STEMS_HOME: stems.toml, cache/ (spotify login, spool), library/
    queue/*.links    pending import batches (one link per line); queue/done/
    run/*.pid  logs/*.log
"""

from __future__ import annotations

import json
import os
import shutil
import signal
import subprocess
import sys
import tarfile
import time
from pathlib import Path

FARM = Path(os.environ.get("FARM_HOME", Path.home() / "stems-farm"))
HOME = FARM / "home"
LIB = HOME / "library"
TRACKS = LIB / "tracks"
SPOOL = HOME / "cache" / "spool"
QUEUE = FARM / "queue"
RUN = FARM / "run"
LOGS = FARM / "logs"
STAGES = "audio,stems,analysis"
# stop fetching new songs while this many wait to be sent, or disk is low
MAX_OUTBOX = int(os.environ.get("STEMS_FARM_MAX_OUTBOX", "1200"))
MIN_FREE_GB = float(os.environ.get("STEMS_FARM_MIN_FREE_GB", "8"))


def env() -> dict:
    e = dict(os.environ)
    e["STEMS_HOME"] = str(HOME)
    e["NO_COLOR"] = "1"
    return e


# ---- library -------------------------------------------------------------


def entries():
    if not TRACKS.is_dir():
        return
    for d in sorted(TRACKS.iterdir()):
        f = d / "track.json"
        if not f.is_file():
            continue
        try:
            yield d, json.loads(f.read_text())
        except (OSError, json.JSONDecodeError):
            continue


def state(e: dict, stage: str) -> str:
    return (e.get("stages") or {}).get(stage, {}).get("state", "pending")


def is_ready(d: Path, e: dict) -> bool:
    """Audio and stems done and the analysis attempted: worth sending."""
    return (
        not (d / ".sent").exists()
        and state(e, "audio") == "done"
        and state(e, "stems") == "done"
        and state(e, "analysis") != "pending"
    )


def files_of(d: Path) -> dict[str, int]:
    return {
        f.name: f.stat().st_size
        for f in sorted(d.iterdir())
        if f.is_file() and not f.name.endswith(".part") and f.name != ".sent"
    }


def summary() -> dict:
    counts = {"tracks": 0, "ready": 0, "sent": 0, "working": 0, "failed": 0}
    failed = []
    for d, e in entries():
        counts["tracks"] += 1
        if (d / ".sent").exists():
            counts["sent"] += 1
        elif is_ready(d, e):
            counts["ready"] += 1
        elif any(state(e, s) == "failed" for s in ("audio", "stems")):
            counts["failed"] += 1
            err = next(
                (e["stages"][s].get("error") for s in ("audio", "stems") if state(e, s) == "failed"), ""
            )
            failed.append(f"{', '.join(e['track'].get('artists') or [])} — {e['track']['title']}: {err}")
        else:
            counts["working"] += 1
    return {**counts, "failures": failed[:20]}


# ---- processes -----------------------------------------------------------


def pid_alive(name: str) -> int | None:
    f = RUN / f"{name}.pid"
    try:
        pid = int(f.read_text())
        os.kill(pid, 0)
        return pid
    except (OSError, ValueError):
        return None


def spawn(name: str, argv: list[str]) -> None:
    if pid_alive(name):
        return
    RUN.mkdir(parents=True, exist_ok=True)
    LOGS.mkdir(parents=True, exist_ok=True)
    log = open(LOGS / f"{name}.log", "ab")
    p = subprocess.Popen(argv, stdout=log, stderr=subprocess.STDOUT, env=env(), start_new_session=True)
    (RUN / f"{name}.pid").write_text(str(p.pid))
    print(f"started {name} (pid {p.pid})")


def start() -> None:
    py = FARM / "venv" / "bin" / "python"
    here = Path(__file__).resolve().parent
    dummy = ["--dummy"] if os.environ.get("STEMS_FARM_DUMMY") else []
    spawn(
        "separator",
        [str(py), str(here / "separator.py"), "--spool", str(SPOOL), "--models", str(FARM / "models"), *dummy],
    )
    spawn("runner", [str(py), str(Path(__file__).resolve()), "_runner"])
    # dashboard on 127.0.0.1:8765 (ssh -L 8765:localhost:8765 <box>)
    spawn("web", [str(py), str(here / "web.py")])


def stop() -> None:
    for name in ("runner", "separator", "web"):
        pid = pid_alive(name)
        if pid:
            # the runner's whole group (it and the stemsd it runs); the
            # separator alone, so it finishes its song and its ffmpeg
            # encoder isn't killed mid-file
            if name == "runner":
                os.killpg(pid, signal.SIGTERM)
            else:
                os.kill(pid, signal.SIGTERM)
            print(f"stopping {name} (pid {pid})")


def free_gb() -> float:
    return shutil.disk_usage(FARM).free / 1e9


def runner() -> None:
    """Import queued batches one after another, pausing while the outbox
    is full or the disk low; retry failures when the queue is empty."""
    QUEUE.mkdir(parents=True, exist_ok=True)
    (QUEUE / "done").mkdir(exist_ok=True)
    stemsd = str(FARM / "bin" / "stemsd")
    retried = False
    while True:
        s = summary()
        if s["ready"] >= MAX_OUTBOX or free_gb() < MIN_FREE_GB:
            print(f"paused: {s['ready']} waiting to be sent, {free_gb():.0f} GB free", flush=True)
            time.sleep(60)
            continue
        batches = sorted(QUEUE.glob("*.links"))
        if batches:
            batch = batches[0]
            links = [l.strip() for l in batch.read_text().splitlines() if l.strip() and not l.startswith("#")]
            print(f"importing {batch.name}: {len(links)} links", flush=True)
            r = subprocess.run([stemsd, "import", "--stages", STAGES, *links], env=env())
            if r.returncode == 0:
                batch.rename(QUEUE / "done" / batch.name)
            else:
                print(f"{batch.name}: stemsd exited {r.returncode}; retrying in 5 min", flush=True)
                time.sleep(300)
            retried = False
            continue
        if not retried:
            # anything that failed (a hiccup, a full GPU…) gets another go
            subprocess.run([stemsd, "import", "--stages", STAGES, "--resume"], env=env())
            retried = True
        time.sleep(15)


# ---- commands ------------------------------------------------------------


def cmd_add(links: list[str]) -> None:
    if not links:
        sys.exit("farm add <spotify links…>")
    QUEUE.mkdir(parents=True, exist_ok=True)
    name = time.strftime("%Y%m%d-%H%M%S") + f"-{os.getpid()}.links"
    (QUEUE / name).write_text("\n".join(links) + "\n")
    print(f"queued {len(links)} link(s) as {name}")
    start()


def cmd_status(as_json: bool) -> None:
    s = summary()
    queued = [p.name for p in sorted(QUEUE.glob("*.links"))] if QUEUE.is_dir() else []
    s.update(
        separator=bool(pid_alive("separator")),
        runner=bool(pid_alive("runner")),
        queued_batches=queued,
        free_gb=round(free_gb(), 1),
        spool_jobs=len(list((SPOOL / "in").glob("*.json"))) if SPOOL.is_dir() else 0,
    )
    if as_json:
        print(json.dumps(s, indent=2))
        return
    print(f"separator: {'running' if s['separator'] else 'stopped'}   runner: {'running' if s['runner'] else 'stopped'}")
    print(f"tracks: {s['tracks']}  ready to send: {s['ready']}  sent: {s['sent']}  in progress: {s['working']}  failed: {s['failed']}")
    print(f"queued batches: {len(queued)}   free disk: {s['free_gb']} GB")
    for f in s["failures"]:
        print(f"  failed: {f}")
    try:
        gpu = subprocess.run(
            ["nvidia-smi", "--query-gpu=name,utilization.gpu,memory.used,memory.total,temperature.gpu", "--format=csv,noheader"],
            capture_output=True,
            text=True,
            timeout=10,
        ).stdout.strip()
        if gpu:
            print(f"gpu: {gpu}")
    except (OSError, subprocess.TimeoutExpired):
        pass


def cmd_outbox() -> None:
    out = []
    for d, e in entries():
        if is_ready(d, e):
            out.append({"key": d.name, "title": e["track"]["title"], "files": files_of(d)})
    playlists = LIB / "playlists"
    print(
        json.dumps(
            {
                "tracks": out,
                "playlists": sorted(p.name for p in playlists.glob("*.json")) if playlists.is_dir() else [],
            }
        )
    )


def cmd_pack(keys: list[str]) -> None:
    """Stream a tar to stdout: tracks/<key>/… for each key, then playlists/."""
    with tarfile.open(fileobj=sys.stdout.buffer, mode="w|") as tar:
        for key in keys:
            d = TRACKS / key
            if "/" in key or key.startswith(".") or not d.is_dir():
                continue
            for name in files_of(d):
                tar.add(d / name, arcname=f"tracks/{key}/{name}")
        playlists = LIB / "playlists"
        if playlists.is_dir():
            for p in sorted(playlists.glob("*.json")):
                tar.add(p, arcname=f"playlists/{p.name}")


def cmd_ack(keys: list[str]) -> None:
    to = os.environ.get("STEMS_FARM_PEER", "antoine-pc")
    done = 0
    for key in keys:
        d = TRACKS / key
        if "/" in key or key.startswith(".") or not (d / "track.json").is_file():
            continue
        (d / ".sent").write_text(f"{to} {time.strftime('%Y-%m-%dT%H:%M:%S')}\n")
        for f in d.iterdir():
            if f.name not in ("track.json", ".sent"):
                shutil.rmtree(f) if f.is_dir() else f.unlink()
        done += 1
    print(json.dumps({"acked": done}))


def cmd_logs(which: str) -> None:
    f = LOGS / f"{which}.log"
    if f.is_file():
        os.execvp("tail", ["tail", "-n", "60", "-f", str(f)])
    sys.exit(f"no log at {f}")


def main() -> None:
    args = sys.argv[1:]
    cmd = args[0] if args else "status"
    rest = args[1:]
    if cmd == "add":
        cmd_add(rest)
    elif cmd == "start":
        start()
    elif cmd == "stop":
        stop()
    elif cmd == "status":
        cmd_status("--json" in rest)
    elif cmd == "outbox":
        cmd_outbox()
    elif cmd == "pack":
        cmd_pack(rest)
    elif cmd == "ack":
        cmd_ack(rest)
    elif cmd == "logs":
        cmd_logs(rest[0] if rest else "runner")
    elif cmd == "_runner":
        runner()
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
