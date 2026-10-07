#!/bin/bash
# Xcode build phase for the Mac app: build stemsd (release, CoreML) and the
# web apps, and copy them into the app bundle so Stems.app runs on its own:
#
#   Stems.app/Contents/MacOS/stemsd
#   Stems.app/Contents/Resources/web/{stage,remote}/dist
#
# STEMS_SKIP_EMBED=1 skips it (the app then falls back to target/release
# in this checkout).
set -euo pipefail

[ "${STEMS_SKIP_EMBED:-}" = 1 ] && { echo "skipping (STEMS_SKIP_EMBED=1)"; exit 0; }

ROOT=$(cd "$SRCROOT/../.." && pwd)
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
# Xcode exports SDK/arch variables that confuse cargo's build scripts
unset SDKROOT MACOSX_DEPLOYMENT_TARGET IPHONEOS_DEPLOYMENT_TARGET ARCHS
unset LD_RUNPATH_SEARCH_PATHS

echo "building stemsd"
(cd "$ROOT" && cargo build --release -p stemsd --features coreml)

if command -v pnpm >/dev/null; then
  for app in stage remote; do
    dir="$ROOT/web/$app"
    index="$dir/dist/index.html"
    if [ ! -f "$index" ] || [ -n "$(find "$dir/src" "$ROOT/web/shared" "$dir/index.html" -newer "$index" -print -quit)" ]; then
      echo "building web/$app"
      [ -d "$dir/node_modules" ] || (cd "$dir" && pnpm install --frozen-lockfile)
      (cd "$dir" && pnpm build)
    fi
  done
else
  echo "warning: pnpm not found; embedding the existing web builds" >&2
fi

dest_bin="$TARGET_BUILD_DIR/$EXECUTABLE_FOLDER_PATH/stemsd"
cp -f "$ROOT/target/release/stemsd" "$dest_bin"
codesign --force --sign "${EXPANDED_CODE_SIGN_IDENTITY:--}" "$dest_bin"

for app in stage remote; do
  src="$ROOT/web/$app/dist"
  dest="$TARGET_BUILD_DIR/$UNLOCALIZED_RESOURCES_FOLDER_PATH/web/$app/dist"
  if [ -f "$src/index.html" ]; then
    rm -rf "$dest" && mkdir -p "$dest"
    cp -R "$src/" "$dest/"
  else
    echo "warning: web/$app is not built; the $app page won't be served" >&2
  fi
done
echo "embedded stemsd and the web apps"
