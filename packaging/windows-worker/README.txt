stems worker for Windows
========================

Separates songs (vocals vs. music) and times lyrics for the Mac running
stems, so the Mac itself stays cool. Nothing is installed: everything,
including what it downloads or writes, stays in this folder. Delete the
folder and it is gone.

Quick test (no Mac or network needed)
-------------------------------------
Double-click bench.cmd. It times the model on the GPU and on the CPU
(2-5 minutes) and prints a verdict for each. "good" is what we want.

Working for the Mac
-------------------
1. On the Mac, in stems.toml under [karaoke]:
       backend = "remote"
       remote = "listen:7375"
       local_fallback = "never"
   and start stemsd. Its log says "waiting for separation workers".
2. Here, double-click run-worker.cmd and type the Mac's IP address once
   (it is remembered in data\player.txt). The window shows
   "connected to player"; the Mac shows "separation worker connected".
3. Leave the window open. Close it to stop.

This laptop connects out to the Mac; nothing connects in, so no firewall
changes or admin rights are needed. While the worker runs the laptop
will not go to sleep (the screen still turns off). Keep it plugged in.

If something goes wrong
-----------------------
- "Windows protected your PC": click "More info", then "Run anyway".
  A work laptop may not allow that at all; then it cannot be the worker.
- bench.cmd shows an error for the GPU but the CPU works: change
  device = "directml" to device = "cpu" in data\stems.toml (slower).
- "out of memory" on the GPU: set workers = 1 in data\stems.toml.
- run-worker.cmd keeps saying "player not reachable yet": check the Mac's
  address, that both are on the same network, and that stemsd runs on
  the Mac with remote = "listen:7375".

Contents
--------
stemsd.exe       stems (only the worker part is used here)
onnxruntime.dll, onnxruntime_providers_shared.dll, DirectML.dll
                 Microsoft ONNX Runtime with DirectML (runs the models)
msvcp140*.dll, vcruntime140*.dll
                 Microsoft Visual C++ runtime, needed by ONNX Runtime
data\            settings, the two models, and anything stems writes
licenses\        licences of the Microsoft components
