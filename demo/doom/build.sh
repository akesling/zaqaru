#!/bin/sh
# Makes the Doom bench: Hubris's Doom app (app-doom.toml, on the `doom`
# branch of Hubris) built, a container that holds it, the shareware WAD,
# Humility and the bench's front door, and that container baked into a
# module whose kernel runs the board.
#
#   demo/doom/build.sh <hubris-checkout> <doom1.wad> [out-dir]     default web/demo
#
# Humility is the Hubris bench's: demo/hubris/build.sh builds it into
# web/demo/hubris-build/bench, or HUMILITY names one. Then serve the
# repository and open
#
#   web/?module=demo/doom.wasm&live=80&terminal=80
#
# Click the screen and play: arrows to move, Ctrl to fire, Space to open,
# Alt to strafe, Shift to run, Esc for the menu, Enter to choose.
#
# Like demo/hubris/build.sh, Hubris is built on this host and the rest as
# Linux x86-64 programs, in a container made from builder.Dockerfile when
# this is not such a host.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
if [ $# -lt 2 ]; then
  echo "usage: demo/doom/build.sh <hubris-checkout> <doom1.wad> [out-dir]" >&2
  exit 2
fi
hubris=$(cd "$1" && pwd)
wad=$(cd "$(dirname "$2")" && pwd)/$(basename "$2")
out=${3:-$repo/web/demo}
mkdir -p "$out"
out=$(cd "$out" && pwd)
work="$out/doom-build"
humility=${HUMILITY:-$out/hubris-build/bench/humility}
if [ ! -x "$humility" ]; then
  echo "build: no Humility at $humility: run demo/hubris/build.sh, or name one with HUMILITY" >&2
  exit 1
fi
mkdir -p "$work/bench/archives"

if [ "$(uname -s)-$(uname -m)" = Linux-x86_64 ]; then
  linux() { Z="$repo" W="$work" T="$repo/target" sh -euc "$1"; }
else
  builder="zaqaru-hubris-builder:$(git hash-object "$repo/demo/hubris/builder.Dockerfile" | cut -c1-12)"
  docker image inspect "$builder" >/dev/null 2>&1 \
    || docker build -q --platform linux/amd64 -t "$builder" -f "$repo/demo/hubris/builder.Dockerfile" "$repo/demo/hubris" >/dev/null
  linux() {
    docker run --rm --platform linux/amd64 \
      -v zaqaru-hubris-cargo:/usr/local/cargo/registry \
      -v "$repo:/z" -v "$work:/w" -e Z=/z -e W=/w -e T=/z/target-linux \
      "$builder" sh -euc "$1"
  }
fi

echo "build: Hubris, app/demo-stm32h7-nucleo/app-doom.toml"
(cd "$hubris" && cargo xtask dist app/demo-stm32h7-nucleo/app-doom.toml)
dist="$hubris/target/doom-stm32h753-nucleo/dist/default"
cp "$dist/final.bin" "$work/bench/final.bin"
cp "$dist/build-doom-stm32h753-nucleo-image-default.zip" "$work/bench/hubris.zip"

echo "build: zaqaru and zaqaru-m0"
linux 'cargo build --release -q --manifest-path "$Z/Cargo.toml" --target-dir "$T" -p zaqaru -p zaqaru-armv6m \
  && cp "$T/release/zaqaru-m0" "$W/bench/zaqaru-m0"'

# Deflated, not bzip2'd, for Humility's sake: see demo/hubris/build.sh.
echo "build: the archive, deflated"
linux '"$T/release/zaqaru-m0" repack "$W/bench/hubris.zip" "$W/bench/hubris.zip.new" \
  && mv "$W/bench/hubris.zip.new" "$W/bench/hubris.zip" && cp "$W/bench/hubris.zip" "$W/bench/archives/doom.zip"'

echo "build: the bench image"
cp "$here/Dockerfile" "$here/board" "$wad" "$work/bench/"
[ "$(basename "$wad")" = doom1.wad ] || mv "$work/bench/$(basename "$wad")" "$work/bench/doom1.wad"
cp "$humility" "$work/bench/humility"
docker build -q --platform linux/amd64 -t zaqaru-doom-bench "$work/bench" >/dev/null
docker save zaqaru-doom-bench -o "$work/bench.tar"

echo "build: the module"
linux '"$T/release/zaqaru" bake "$W/bench.tar" -o "$W/doom.wasm"'
mv "$work/doom.wasm" "$out/doom.wasm"
rm -f "$work/bench.tar"
echo "bench in $out/doom.wasm"
echo "serve the repository and open web/?module=$(basename "$out")/doom.wasm&live=80&terminal=80"
