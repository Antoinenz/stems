#!/usr/bin/env bash
# Set up a Linux GPU box (Ubuntu/Debian, NVIDIA, e.g. a rented 4090) as the
# stems farm's separation machine. Safe to re-run: it updates in place.
#
#   curl -fsSL https://raw.githubusercontent.com/Antoinenz/stems/main/farm/cloud/setup.sh | bash
#   # or, from a checkout: farm/cloud/setup.sh
#
# Then copy your Spotify login (from a machine where stems already works;
# librespot keeps it in a folder named credentials.json):
#   scp -r ~/Library/Caches/stems/spotify/credentials.json \
#       <box>:~/stems-farm/home/cache/spotify/
# and queue music:  farm add https://open.spotify.com/playlist/…
#
# Env: FARM_HOME (default ~/stems-farm), STEMS_FARM_MODEL (separation model).
set -euo pipefail

FARM_HOME=${FARM_HOME:-$HOME/stems-farm}
REPO_URL=${STEMS_REPO:-https://github.com/Antoinenz/stems.git}
MODEL=${STEMS_FARM_MODEL:-model_bs_roformer_ep_317_sdr_12.9755.ckpt}
ESSENTIA_BASE=https://essentia.upf.edu/models
mkdir -p "$FARM_HOME"/{bin,models/essentia,home/cache/spotify,home/cache/spool,queue/done,run,logs}

SUDO=""
[ "$(id -u)" = 0 ] || SUDO="sudo"

echo "==> system packages"
export DEBIAN_FRONTEND=noninteractive
$SUDO apt-get update -qq
$SUDO apt-get install -y -qq build-essential pkg-config libasound2-dev git curl ca-certificates ffmpeg >/dev/null

echo "==> Rust"
if ! command -v cargo >/dev/null && [ ! -x "$HOME/.cargo/bin/cargo" ]; then
  curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal >/dev/null
fi
export PATH="$HOME/.cargo/bin:$PATH"

echo "==> stems source"
if [ -d "$FARM_HOME/repo/.git" ]; then
  git -C "$FARM_HOME/repo" pull --ff-only -q
else
  git clone -q "$REPO_URL" "$FARM_HOME/repo"
fi

echo "==> stemsd (no ONNX: separation runs in the Python daemon)"
(cd "$FARM_HOME/repo" && cargo build -q --release -p stemsd --no-default-features)
cp -f "$FARM_HOME/repo/target/release/stemsd" "$FARM_HOME/bin/stemsd"

echo "==> Python environment (uv, Python 3.11)"
if ! command -v uv >/dev/null && [ ! -x "$HOME/.local/bin/uv" ]; then
  curl -fsSL https://astral.sh/uv/install.sh | sh >/dev/null
fi
export PATH="$HOME/.local/bin:$PATH"
[ -x "$FARM_HOME/venv/bin/python" ] || uv venv -q --python 3.11 "$FARM_HOME/venv"
VIRTUAL_ENV="$FARM_HOME/venv" uv pip install -q "audio-separator[gpu]" essentia-tensorflow soundfile numpy

echo "==> analysis models (Essentia, Discogs-EffNet)"
fetch() { # url dest: download once; a missing optional model is not fatal
  [ -s "$2" ] && return 0
  if curl -fsSL -o "$2.part" "$1"; then
    mv -f "$2.part" "$2"
  else
    rm -f "$2.part"
    echo "    (not available: $(basename "$2"))"
  fi
}
fetch "$ESSENTIA_BASE/feature-extractors/discogs-effnet/discogs-effnet-bs64-1.pb" "$FARM_HOME/models/essentia/discogs-effnet-bs64-1.pb"
for head in genre_discogs400/genre_discogs400 danceability/danceability \
  mood_happy/mood_happy mood_sad/mood_sad mood_party/mood_party mood_relaxed/mood_relaxed \
  mood_aggressive/mood_aggressive mood_acoustic/mood_acoustic mood_electronic/mood_electronic \
  voice_instrumental/voice_instrumental; do
  name=$(basename "$head")-discogs-effnet-1
  for ext in pb json; do
    fetch "$ESSENTIA_BASE/classification-heads/$(dirname "$head")/$name.$ext" "$FARM_HOME/models/essentia/$name.$ext"
  done
done
ls "$FARM_HOME/models/essentia" | sed 's/^/    /'

echo "==> separation model $MODEL"
"$FARM_HOME/venv/bin/python" - "$MODEL" "$FARM_HOME/models" <<'PY'
import logging, sys
from audio_separator.separator import Separator
s = Separator(model_file_dir=sys.argv[2], log_level=logging.WARNING)
s.load_model(model_filename=sys.argv[1])
print("    loaded", sys.argv[1])
PY

echo "==> stems config"
CONF="$FARM_HOME/home/stems.toml"
if [ ! -f "$CONF" ]; then
  cat >"$CONF" <<EOF
# stems on the farm's GPU box: fetch audio, hand it to the spool daemon
# (separation + analysis), nothing else. antoine-pc does lyrics and video.
sink = "null"
port = 7373

[spotify]
bitrate = 320

[karaoke]
backend = "spool"
spool_dir = "$FARM_HOME/home/cache/spool"

[lyrics]
align = false

[video]
enabled = false
EOF
fi

cat >"$FARM_HOME/farm" <<EOF
#!/usr/bin/env bash
export FARM_HOME="$FARM_HOME"
exec "$FARM_HOME/venv/bin/python" "$FARM_HOME/repo/farm/cloud/farm.py" "\$@"
EOF
chmod +x "$FARM_HOME/farm"
$SUDO ln -sf "$FARM_HOME/farm" /usr/local/bin/farm 2>/dev/null || true

echo
nvidia-smi --query-gpu=name,memory.total --format=csv,noheader 2>/dev/null | sed 's/^/GPU: /' || echo "no NVIDIA GPU visible"
df -h "$FARM_HOME" | tail -1 | awk '{print "disk: " $4 " free"}'
if [ ! -s "$FARM_HOME/home/cache/spotify/credentials.json/credentials.json" ]; then
  echo "NEXT: copy your Spotify login to $FARM_HOME/home/cache/spotify/ (see the top of this script)"
fi
echo "ready: farm add <spotify links>   then   farm status"
