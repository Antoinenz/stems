#!/bin/sh
# Build dav1d (VideoLAN's AV1 decoder, BSD-2-Clause) as a universal static
# library for the Mac app, which uses it to play AV1 music videos on Macs
# without a hardware AV1 decoder (everything before M3).
#
# Output: Vendor/dav1d/{include/dav1d/*.h, lib/libdav1d.a, COPYING}
# Needs: meson, ninja, nasm (brew install meson ninja nasm).
set -eu

VERSION=1.5.4
# from https://downloads.videolan.org/pub/videolan/dav1d/$VERSION/dav1d-$VERSION.tar.xz.sha256
SHA256=686616b7c69eb88d44459391ab25cac13b6647a3b288835c5784e71c1514a5c5
MIN_MACOS=26.0

here=$(cd "$(dirname "$0")/.." && pwd)
out="$here/Vendor/dav1d"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

cd "$work"
curl -fsSL -o dav1d.tar.xz "https://downloads.videolan.org/pub/videolan/dav1d/$VERSION/dav1d-$VERSION.tar.xz"
got=$(shasum -a 256 dav1d.tar.xz | cut -d' ' -f1)
if [ "$got" != "$SHA256" ]; then
    echo "checksum mismatch for dav1d $VERSION: got $got" >&2
    exit 1
fi
tar xJf dav1d.tar.xz
src="$work/dav1d-$VERSION"

build() {
    arch=$1 cpu=$2
    cat > "cross-$arch.txt" <<EOF
[binaries]
c = ['clang', '-arch', '$arch', '-mmacosx-version-min=$MIN_MACOS']
ar = 'ar'
strip = 'strip'
nasm = 'nasm'

[host_machine]
system = 'darwin'
cpu_family = '$cpu'
cpu = '$cpu'
endian = 'little'
EOF
    meson setup "build-$arch" "$src" --cross-file "cross-$arch.txt" \
        --buildtype release --default-library static \
        -Denable_tools=false -Denable_tests=false -Denable_examples=false \
        -Dbitdepths=8,16 >/dev/null
    ninja -C "build-$arch" >/dev/null
}

build arm64 aarch64
build x86_64 x86_64

rm -rf "$out"
mkdir -p "$out/lib" "$out/include/dav1d"
lipo -create build-arm64/src/libdav1d.a build-x86_64/src/libdav1d.a -output "$out/lib/libdav1d.a"
cp "$src"/include/dav1d/*.h "$out/include/dav1d/"
cp "$src/COPYING" "$out/COPYING"
cat > "$out/include/module.modulemap" <<'EOF'
module CDav1d {
    header "dav1d/dav1d.h"
    link "dav1d"
    export *
}
EOF
echo "dav1d $VERSION → $out ($(du -h "$out/lib/libdav1d.a" | cut -f1))"
