#!/usr/bin/env bash
# Package a portable stems worker for Windows, built on macOS (or Linux):
#
#   scripts/package-windows-worker.sh [arm64|x64]
#
# Output: dist/stems-worker-win-<arch>/ and a .zip of it, holding stemsd.exe
# (cross-compiled with cargo-xwin), ONNX Runtime with DirectML, the Visual C++
# runtime DLLs ONNX Runtime needs, both models, and run-worker.cmd /
# bench.cmd. Nothing has to be installed on the Windows machine.
#
# Needs: the rustup target, cargo-xwin, LLVM (clang, lld-link, llvm-readobj),
# 7zz, curl, unzip, zip, python3. Models come from this machine's stems cache
# (run `stemsd bench` once to download them) or STEMS_MODELS_DIR.
set -euo pipefail

ARCH=${1:-arm64}
ORT_VERSION=1.24.4 # newest ONNX Runtime with DirectML on NuGet
DML_VERSION=1.15.4
case "$ARCH" in
  arm64) TARGET=aarch64-pc-windows-msvc RID=win-arm64 DML_DIR=arm64-win VC=arm64 PE_ARCH=Aarch64 ;;
  x64) TARGET=x86_64-pc-windows-msvc RID=win-x64 DML_DIR=x64-win VC=x64 PE_ARCH=x86-64 ;;
  *) echo "usage: $0 [arm64|x64]" >&2 && exit 2 ;;
esac
VC_DLLS=(msvcp140.dll msvcp140_1.dll vcruntime140.dll)
# x64 code also needs the second runtime DLL for exception handling
[ "$ARCH" = x64 ] && VC_DLLS+=(vcruntime140_1.dll)
MODELS_NEEDED=(UVR-MDX-NET-Inst_HQ_3 wav2vec2-base-960h)

ROOT=$(cd "$(dirname "$0")/.." && pwd)
WORK=$ROOT/target/winpkg
NAME=stems-worker-win-$ARCH
OUT=$ROOT/dist/$NAME
if [ -z "${STEMS_MODELS_DIR:-}" ]; then
  case "$(uname)" in
    Darwin) STEMS_MODELS_DIR=$HOME/Library/Caches/stems/models ;;
    *) STEMS_MODELS_DIR=${XDG_CACHE_HOME:-$HOME/.cache}/stems/models ;;
  esac
fi
for m in "${MODELS_NEEDED[@]}"; do
  [ -f "$STEMS_MODELS_DIR/$m.onnx" ] || {
    echo "missing $STEMS_MODELS_DIR/$m.onnx: run \`stemsd bench\` once to download it, or set STEMS_MODELS_DIR" >&2
    exit 1
  }
done

for d in /opt/homebrew/opt/llvm/bin /opt/homebrew/opt/lld/bin /usr/local/opt/llvm/bin /usr/local/opt/lld/bin; do
  [ -d "$d" ] && PATH="$d:$PATH"
done
for tool in cargo-xwin clang lld-link llvm-readobj 7zz curl unzip zip python3; do
  command -v "$tool" >/dev/null || { echo "missing tool: $tool" >&2 && exit 1; }
done

fetch() { # url dest: download once
  if [ ! -s "$2" ]; then
    mkdir -p "$(dirname "$2")"
    curl -sSfL -o "$2.part" "$1"
    mv "$2.part" "$2"
  fi
}

echo "==> stemsd.exe for $TARGET"
# a static C runtime: stemsd.exe itself needs no Visual C++ DLLs
# (cargo-xwin appends its own flags to this)
TARGET_ENV=$(echo "$TARGET" | tr 'a-z-' 'A-Z_')
export "CARGO_TARGET_${TARGET_ENV}_RUSTFLAGS=-C target-feature=+crt-static"
(cd "$ROOT" && cargo xwin build --cross-compiler clang --release -p stemsd \
  --target "$TARGET" --no-default-features --features onnx-dylib,directml)

echo "==> ONNX Runtime $ORT_VERSION (DirectML) and DirectML $DML_VERSION"
ORT_PKG=$WORK/nuget/ort-dml-$ORT_VERSION
DML_PKG=$WORK/nuget/directml-$DML_VERSION
fetch "https://api.nuget.org/v3-flatcontainer/microsoft.ml.onnxruntime.directml/$ORT_VERSION/microsoft.ml.onnxruntime.directml.$ORT_VERSION.nupkg" "$ORT_PKG.nupkg"
fetch "https://api.nuget.org/v3-flatcontainer/microsoft.ai.directml/$DML_VERSION/microsoft.ai.directml.$DML_VERSION.nupkg" "$DML_PKG.nupkg"
if [ ! -d "$ORT_PKG" ]; then
  mkdir -p "$ORT_PKG"
  unzip -q -o "$ORT_PKG.nupkg" 'runtimes/win-*/native/*.dll' LICENSE ThirdPartyNotices.txt -d "$ORT_PKG"
fi
if [ ! -d "$DML_PKG" ]; then
  mkdir -p "$DML_PKG"
  unzip -q -o "$DML_PKG.nupkg" 'bin/*-win/DirectML.dll' 'LICENSE*.txt' -d "$DML_PKG"
fi

echo "==> Visual C++ runtime ($VC)"
# The official redistributable installer carries the DLLs in cabinet files
# appended to it; Microsoft allows shipping them next to an application.
VCX=$WORK/vcredist/$VC
if [ ! -d "$VCX/files" ]; then
  fetch "https://aka.ms/vs/17/release/vc_redist.$VC.exe" "$WORK/vcredist/vc_redist.$VC.exe"
  rm -rf "$VCX" && mkdir -p "$VCX/cabs" "$VCX/files"
  python3 -I - "$WORK/vcredist/vc_redist.$VC.exe" "$VCX/cabs" <<'PY'
import os, struct, sys
data = open(sys.argv[1], "rb").read()
i = n = 0
while (i := data.find(b"MSCF\0\0\0\0", i)) >= 0:
    size = struct.unpack_from("<I", data, i + 8)[0]
    if 0 < size <= len(data) - i:
        open(os.path.join(sys.argv[2], f"part{n}.cab"), "wb").write(data[i:i + size])
        n, i = n + 1, i + size
    else:
        i += 4
PY
  # the payload cabinet holds an MSI and a nested cabinet with the DLLs
  for c in "$VCX"/cabs/*.cab; do
    7zz x -y -o"$VCX/cabs/$(basename "$c" .cab)" "$c" >/dev/null
  done
  for f in "$VCX"/cabs/*/*; do
    if [ "$(head -c 4 "$f")" = "MSCF" ]; then
      7zz x -y -o"$VCX/files" "$f" >/dev/null
    fi
  done
fi
pick_vc() { # dll name -> the file of that name built for $PE_ARCH
  for f in "$VCX"/files/"$1"_*; do
    if file -b "$f" | grep -q "$PE_ARCH"; then
      echo "$f"
      return
    fi
  done
  echo "no $1 for $ARCH in the Visual C++ redistributable" >&2
  exit 1
}

echo "==> assembling $OUT"
rm -rf "$OUT" && mkdir -p "$OUT/data/cache/models" "$OUT/licenses"
cp "$ROOT/target/$TARGET/release/stemsd.exe" "$OUT/"
cp "$ORT_PKG/runtimes/$RID/native/onnxruntime.dll" \
  "$ORT_PKG/runtimes/$RID/native/onnxruntime_providers_shared.dll" "$OUT/"
cp "$DML_PKG/bin/$DML_DIR/DirectML.dll" "$OUT/"
for dll in "${VC_DLLS[@]}"; do
  cp "$(pick_vc "$dll")" "$OUT/$dll"
done
for m in "${MODELS_NEEDED[@]}"; do
  cp "$STEMS_MODELS_DIR/$m.onnx" "$OUT/data/cache/models/"
done
crlf() { sed 's/$/\r/' "$1" >"$2"; } # batch files want Windows line endings
for f in run-worker.cmd bench.cmd README.txt; do
  crlf "$ROOT/packaging/windows-worker/$f" "$OUT/$f"
done
crlf "$ROOT/packaging/windows-worker/stems.toml" "$OUT/data/stems.toml"
cp "$ORT_PKG/LICENSE" "$OUT/licenses/onnxruntime-LICENSE.txt"
cp "$ORT_PKG/ThirdPartyNotices.txt" "$OUT/licenses/onnxruntime-ThirdPartyNotices.txt"
cp "$DML_PKG/LICENSE.txt" "$OUT/licenses/DirectML-LICENSE.txt"
cp "$ROOT/LICENSE" "$OUT/licenses/stems-LICENSE.txt"
{
  echo "stems worker for Windows $ARCH"
  echo "stems $(git -C "$ROOT" describe --always --dirty), built $(date -u +%Y-%m-%dT%H:%MZ)"
  echo "ONNX Runtime $ORT_VERSION (DirectML), DirectML $DML_VERSION"
} | sed 's/$/\r/' >"$OUT/VERSION.txt"

echo "==> checking that every DLL the worker loads is here or part of Windows"
bundled=$(ls "$OUT" | tr '[:upper:]' '[:lower:]')
missing=0
for bin in "$OUT"/*.exe "$OUT"/*.dll; do
  for dep in $(llvm-readobj --coff-imports "$bin" | awk '/Name:/ {print $2}'); do
    lower=$(echo "$dep" | tr '[:upper:]' '[:lower:]')
    case "$lower" in
      msvcp* | vcruntime* | concrt* | onnxruntime* | directml.dll)
        if ! grep -qx "$lower" <<<"$bundled"; then
          echo "  $(basename "$bin") needs $dep, which is not bundled" >&2
          missing=1
        fi
        ;;
    esac
  done
done
[ "$missing" = 0 ] || exit 1

(cd "$ROOT/dist" && rm -f "$NAME.zip" && zip -qr "$NAME.zip" "$NAME")
echo "==> dist/$NAME.zip ($(du -h "$ROOT/dist/$NAME.zip" | cut -f1))"
