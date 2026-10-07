#!/usr/bin/env python3
"""A small dashboard for the GPU box: load, progress, ETA, recent songs, and
a box to queue more links. Standard library only. Listens on 127.0.0.1 —
reach it through ssh:

    ssh -L 8765:localhost:8765 <box>      then open http://localhost:8765
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import farm  # noqa: E402

PORT = int(os.environ.get("STEMS_FARM_WEB_PORT", "8765"))
SONG_LINE = re.compile(
    r"^(\S+ \S+),\d+ INFO (.+?): (\d+) s of audio in ([\d.]+) s \(([\d.]+)x\)$"
)


def recent_songs(limit: int = 400) -> list[dict]:
    f = farm.LOGS / "separator.log"
    if not f.is_file():
        return []
    with open(f, "rb") as fh:
        fh.seek(0, 2)
        fh.seek(max(0, fh.tell() - 400_000))
        lines = fh.read().decode(errors="replace").splitlines()
    out = []
    for line in lines:
        m = SONG_LINE.match(line)
        if m:
            out.append(
                {
                    "at": m.group(1),
                    "title": m.group(2),
                    "audio_s": int(m.group(3)),
                    "took_s": float(m.group(4)),
                    "speed": float(m.group(5)),
                }
            )
    return out[-limit:]


def cpu_percent() -> float:
    def read():
        with open("/proc/stat") as f:
            v = [int(x) for x in f.readline().split()[1:]]
        return v[3] + v[4], sum(v)

    i1, t1 = read()
    time.sleep(0.2)
    i2, t2 = read()
    return round(100 * (1 - (i2 - i1) / max(1, t2 - t1)), 1)


def memory() -> dict:
    info = {}
    with open("/proc/meminfo") as f:
        for line in f:
            k, v = line.split(":")
            info[k] = int(v.split()[0]) * 1024
    return {"used": info["MemTotal"] - info["MemAvailable"], "total": info["MemTotal"]}


def gpu() -> dict | None:
    try:
        out = subprocess.run(
            [
                "nvidia-smi",
                "--query-gpu=name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw",
                "--format=csv,noheader,nounits",
            ],
            capture_output=True,
            text=True,
            timeout=5,
        ).stdout.strip()
        name, util, used, total, temp, power = [x.strip() for x in out.split(",")]
        return {
            "name": name,
            "util": float(util),
            "mem_used_mb": float(used),
            "mem_total_mb": float(total),
            "temp_c": float(temp),
            "power_w": float(power) if power not in ("[N/A]", "") else None,
        }
    except Exception:  # noqa: BLE001 - no GPU / no driver
        return None


def playlists_progress(states: dict[str, str]) -> list[dict]:
    out = []
    pl = farm.LIB / "playlists"
    for p in sorted(pl.glob("*.json")) if pl.is_dir() else []:
        try:
            j = json.loads(p.read_text())
        except (OSError, ValueError):
            continue
        keys = j.get("tracks", [])
        c = {"done": 0, "failed": 0, "working": 0, "waiting": 0}
        for k in keys:
            c[states.get(k, "waiting")] += 1
        out.append({"name": j.get("name", p.stem), "url": j.get("url"), "total": len(keys), **c})
    return out


def snapshot() -> dict:
    states: dict[str, str] = {}
    failures = []
    current = None
    for d, e in farm.entries():
        if (d / ".sent").exists() or farm.is_ready(d, e):
            states[d.name] = "done"
        elif any(farm.state(e, s) == "failed" for s in ("audio", "stems")):
            states[d.name] = "failed"
            err = next(
                (e["stages"][s].get("error", "") for s in ("audio", "stems") if farm.state(e, s) == "failed"), ""
            )
            failures.append({"title": f"{', '.join(e['track'].get('artists') or [])} — {e['track']['title']}", "error": err})
        else:
            states[d.name] = "working"
    # what the separator is on right now
    work = farm.SPOOL / "work"
    for j in work.glob("*.json") if work.is_dir() else []:
        try:
            spec = json.loads(j.read_text())
            current = {"title": f"{', '.join(spec.get('artists') or [])} — {spec.get('title')}", "since": j.stat().st_mtime}
        except (OSError, ValueError):
            pass
    lists = playlists_progress(states)
    wanted = set()
    for p in (farm.LIB / "playlists").glob("*.json") if (farm.LIB / "playlists").is_dir() else []:
        try:
            wanted.update(json.loads(p.read_text()).get("tracks", []))
        except (OSError, ValueError):
            pass
    done = sum(1 for k in wanted if states.get(k) == "done")
    failed = sum(1 for k in wanted if states.get(k) == "failed")
    remaining = len(wanted) - done - failed
    songs = recent_songs()
    # pace from the gaps between the last 15 finished songs (includes the
    # download and analysis, not just the separation)
    eta_s = None
    if len(songs) >= 3:
        stamps = [time.mktime(time.strptime(s["at"], "%Y-%m-%d %H:%M:%S")) for s in songs[-15:]]
        gaps = [b - a for a, b in zip(stamps, stamps[1:]) if 0 < b - a < 600]
        if gaps:
            eta_s = remaining * (sum(gaps) / len(gaps))
    s = farm.summary()
    queued = []
    for b in sorted(farm.QUEUE.glob("*.links")) if farm.QUEUE.is_dir() else []:
        queued.append({"name": b.name, "links": [l for l in b.read_text().splitlines() if l.strip()]})
    return {
        "now": time.time(),
        "separator": bool(farm.pid_alive("separator")),
        "runner": bool(farm.pid_alive("runner")),
        "counts": {k: s[k] for k in ("tracks", "ready", "sent", "working", "failed")},
        "wanted": len(wanted),
        "done": done,
        "remaining": remaining,
        "eta_s": eta_s,
        "current": current,
        "recent": list(reversed(songs[-12:])),
        "avg_speed": round(sum(x["speed"] for x in songs[-15:]) / len(songs[-15:]), 1) if songs else None,
        "playlists": lists,
        "failures": failures[-30:],
        "queued": queued,
        "cpu": cpu_percent(),
        "mem": memory(),
        "gpu": gpu(),
        "disk_free_gb": round(farm.free_gb(), 1),
    }


PAGE = r"""<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>stems farm</title><style>
:root{color-scheme:dark;--bg:#0f1115;--card:#181b22;--dim:#8b93a3;--fg:#e8ebf1;--acc:#7c9cff;--ok:#4ade80;--warn:#fbbf24;--bad:#f87171}
*{box-sizing:border-box}body{margin:0;font:14px/1.45 -apple-system,system-ui,sans-serif;background:var(--bg);color:var(--fg)}
main{max-width:1100px;margin:auto;padding:20px;display:grid;gap:14px;grid-template-columns:repeat(auto-fit,minmax(320px,1fr))}
h1{grid-column:1/-1;margin:0;font-size:20px;display:flex;gap:10px;align-items:center}
.card{background:var(--card);border-radius:14px;padding:16px}.card h2{margin:0 0 10px;font-size:13px;color:var(--dim);text-transform:uppercase;letter-spacing:.06em}
.big{font-size:30px;font-weight:700}.dim{color:var(--dim)}.row{display:flex;justify-content:space-between;gap:10px;margin:4px 0}
.bar{height:8px;border-radius:4px;background:#2a2f3a;overflow:hidden;margin:6px 0 2px}.bar i{display:block;height:100%;background:var(--acc)}
.dot{width:9px;height:9px;border-radius:50%;display:inline-block}.ok{background:var(--ok)}.bad{background:var(--bad)}
table{width:100%;border-collapse:collapse}td{padding:4px 0;border-bottom:1px solid #232733;vertical-align:top}td.r{text-align:right;white-space:nowrap;color:var(--dim)}
textarea{width:100%;min-height:90px;background:#11141a;color:var(--fg);border:1px solid #2a2f3a;border-radius:10px;padding:10px;font:13px ui-monospace,monospace}
button{background:var(--acc);color:#0b0d12;border:0;border-radius:10px;padding:9px 16px;font-weight:600;cursor:pointer;margin-top:8px}
.wide{grid-column:1/-1}.err{color:var(--bad);font-size:12px}small{color:var(--dim)}
</style></head><body><main>
<h1>stems farm <span id="health"></span></h1>
<section class="card"><h2>Progress</h2><div class="big" id="done">–</div><div class="bar"><i id="donebar" style="width:0"></i></div>
<div class="row"><span class="dim" id="eta">ETA –</span><span class="dim" id="speed"></span></div>
<div class="row"><span>Sent to antoine-pc</span><b id="sent">–</b></div><div class="row"><span>Waiting to send</span><b id="ready">–</b></div>
<div class="row"><span>Failed</span><b id="failed">–</b></div></section>
<section class="card"><h2>Now</h2><div id="current" class="dim">–</div><div style="margin-top:12px" id="load"></div></section>
<section class="card"><h2>Add playlists, albums or songs</h2><textarea id="links" placeholder="https://open.spotify.com/playlist/…  (one per line)"></textarea>
<button onclick="add()">Queue</button> <small id="addmsg"></small><div style="margin-top:10px" id="queued"></div></section>
<section class="card wide"><h2>Playlists</h2><table id="lists"></table></section>
<section class="card wide"><h2>Recently finished</h2><table id="recent"></table></section>
<section class="card wide" id="failbox" hidden><h2>Failed (retried when the queue is empty)</h2><table id="fails"></table></section>
</main><script>
const $=id=>document.getElementById(id), esc=s=>String(s??'').replace(/[&<>"]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
const dur=s=>{if(s==null)return'–';s=Math.round(s);const h=Math.floor(s/3600),m=Math.floor(s%3600/60);return h?`${h} h ${m} min`:m?`${m} min`:`${s} s`};
const pct=(a,b)=>b?Math.round(100*a/b):0, gb=b=>(b/1e9).toFixed(1)+' GB';
function bar(label,v,max,text){return `<div class="row"><span>${label}</span><span class="dim">${text}</span></div><div class="bar"><i style="width:${pct(v,max)}%"></i></div>`}
async function tick(){
 try{const s=await (await fetch('api/status')).json();
 $('health').innerHTML=`<span class="dot ${s.separator&&s.runner?'ok':'bad'}" title="separator ${s.separator?'up':'down'}, runner ${s.runner?'up':'down'}"></span>`;
 $('done').textContent=`${s.done} / ${s.wanted} songs`; $('donebar').style.width=pct(s.done,s.wanted)+'%';
 $('eta').textContent=s.remaining>0?`ETA ${dur(s.eta_s)} · ${s.remaining} to go`:'All done';
 $('speed').textContent=s.avg_speed?`${s.avg_speed}× real time`:'';
 $('sent').textContent=s.counts.sent; $('ready').textContent=s.counts.ready; $('failed').textContent=s.counts.failed;
 $('current').innerHTML=s.current?`<b>${esc(s.current.title)}</b><br><small>for ${dur(s.now-s.current.since)}</small>`:'Waiting for the next song (downloading or looking up)';
 const g=s.gpu;
 $('load').innerHTML=(g?bar('GPU '+esc(g.name),g.util,100,`${g.util}% · ${g.temp_c}°C${g.power_w?` · ${Math.round(g.power_w)} W`:''}`)+bar('VRAM',g.mem_used_mb,g.mem_total_mb,`${(g.mem_used_mb/1024).toFixed(1)} / ${(g.mem_total_mb/1024).toFixed(0)} GB`):'')
  +bar('CPU',s.cpu,100,s.cpu+'%')+bar('Memory',s.mem.used,s.mem.total,`${gb(s.mem.used)} / ${gb(s.mem.total)}`)+`<div class="row"><span>Disk free</span><span class="dim">${s.disk_free_gb} GB</span></div>`;
 $('lists').innerHTML=s.playlists.map(p=>`<tr><td>${esc(p.name)}<div class="bar"><i style="width:${pct(p.done,p.total)}%"></i></div></td><td class="r">${p.done} / ${p.total}${p.failed?` · <span class="err">${p.failed} failed</span>`:''}</td></tr>`).join('')||'<tr><td class="dim">None yet</td></tr>';
 $('recent').innerHTML=s.recent.map(r=>`<tr><td>${esc(r.title)}</td><td class="r">${dur(r.audio_s)} in ${Math.round(r.took_s)} s · ${r.speed}×</td><td class="r">${r.at.slice(11,16)} UTC</td></tr>`).join('');
 $('failbox').hidden=!s.failures.length; $('fails').innerHTML=s.failures.map(f=>`<tr><td>${esc(f.title)}</td><td class="r err">${esc(f.error)}</td></tr>`).join('');
 $('queued').innerHTML=s.queued.length?'<small>Queued batches:</small>'+s.queued.map(b=>`<div><small>${esc(b.name)} — ${b.links.length} link(s)</small></div>`).join(''):'';
 }catch(e){$('health').innerHTML='<span class="dot bad"></span> <small>can’t reach the box</small>'}
}
async function add(){const links=$('links').value.split(/\s+/).filter(l=>l.startsWith('http')||l.startsWith('spotify:'));
 if(!links.length){$('addmsg').textContent='No links found';return}
 const r=await fetch('api/add',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({links})});
 const j=await r.json(); $('addmsg').textContent=j.ok?`Queued ${j.count} link(s)`:j.error; if(j.ok)$('links').value=''; tick()}
tick(); setInterval(tick,5000);
</script></body></html>"""


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def send(self, code: int, body: bytes, ctype: str) -> None:
        self.send_response(code)
        self.send_header("content-type", ctype)
        self.send_header("cache-control", "no-store")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path in ("/", "/index.html"):
            self.send(200, PAGE.encode(), "text/html; charset=utf-8")
        elif self.path == "/api/status":
            self.send(200, json.dumps(snapshot()).encode(), "application/json")
        else:
            self.send(404, b"not found", "text/plain")

    def do_POST(self):
        if self.path != "/api/add":
            return self.send(404, b"not found", "text/plain")
        try:
            body = json.loads(self.rfile.read(int(self.headers.get("content-length", 0))))
            links = [
                l.strip()
                for l in body.get("links", [])
                if re.match(r"^(https://open\.spotify\.com/|spotify:)", l.strip())
            ]
            if not links:
                raise ValueError("no Spotify links")
            farm.QUEUE.mkdir(parents=True, exist_ok=True)
            name = time.strftime("%Y%m%d-%H%M%S") + "-web.links"
            (farm.QUEUE / name).write_text("\n".join(links) + "\n")
            reply = {"ok": True, "count": len(links)}
        except Exception as e:  # noqa: BLE001 - shown in the page
            reply = {"ok": False, "error": str(e)}
        self.send(200, json.dumps(reply).encode(), "application/json")


if __name__ == "__main__":
    print(f"dashboard on http://127.0.0.1:{PORT}", flush=True)
    ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
