#!/bin/sh
# Makes the Hubris bench: Hubris's STM32H753 demo built, a container that
# holds it with Humility and a front door, and that container baked into a
# module whose kernel runs the board. Then checks the module under Node.
# Wants docker, node, a Hubris checkout, and some minutes the first time
# (Humility is a large build).
#
#   demo/hubris/build.sh <hubris-checkout> [out-dir]      default web/demo
#
# Then serve the repository and open
#
#   web/?module=demo/hubris.wasm&live=80&terminal=80
#
# and type `humility tasks` (or `help`) into the terminal.
#
# Hubris is built on this host with its own toolchain. The rest has to be
# Linux x86-64 programs, and zaqaru builds only on Linux x86-64, so on any
# other host those steps run in a container made from builder.Dockerfile.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
if [ $# -lt 1 ]; then
  echo "usage: demo/hubris/build.sh <hubris-checkout> [out-dir]" >&2
  exit 2
fi
hubris=$(cd "$1" && pwd)
out=${2:-$repo/web/demo}
mkdir -p "$out"
out=$(cd "$out" && pwd)
work="$out/hubris-build"
mkdir -p "$work/bench"

# Humility at a known commit, as Oxide ships it: nothing patched, default
# features, so probe-rs finds the board's USB probe by itself.
humility_commit=28c8454dee325a3f6a799feeecd9329c9fa8d657

# A registry lookup that times out is worth a second try, and a third.
retry() {
  "$@" || { sleep 5; "$@"; } || { sleep 15; "$@"; }
}

# Runs a shell command as Linux x86-64, with $Z the repository, $W the work
# directory and $T the target directory, wherever those are for it.
if [ "$(uname -s)-$(uname -m)" = Linux-x86_64 ]; then
  native=1
  linux() { Z="$repo" W="$work" T="$repo/target" sh -euc "$1"; }
else
  native=0
  # Named for its recipe, and built only when that changes: resolving the
  # base image against the registry is the slowest and least reliable
  # step there is, and nothing about it changes from one run to the next.
  builder="zaqaru-hubris-builder:$(git hash-object "$here/builder.Dockerfile" | cut -c1-12)"
  docker image inspect "$builder" >/dev/null 2>&1 \
    || retry docker build -q --platform linux/amd64 -t "$builder" -f "$here/builder.Dockerfile" "$here" >/dev/null
  linux() {
    docker run --rm --platform linux/amd64 \
      -v zaqaru-hubris-cargo:/usr/local/cargo/registry \
      -v "$repo:/z" -v "$work:/w" -e Z=/z -e W=/w -e T=/z/target-linux \
      "$builder" sh -euc "$1"
  }
fi

# The board boots the H753 demo; the G031 and G070 demos — the G070's
# pings over USART1 — are there to flash onto it (`flash g070` at the
# bench's prompt), and so is anything else a user uploads.
mkdir -p "$work/bench/archives"
for app in g031 g070 h753; do
  case $app in
    h7*) dir=demo-stm32h7-nucleo ;;
    *) dir=demo-stm32g0-nucleo ;;
  esac
  echo "build: Hubris, app-$app.toml"
  (cd "$hubris" && cargo xtask dist "app/$dir/app-$app.toml")
  cp "$hubris/target/demo-stm32${app}-nucleo/dist/default/build-demo-stm32${app}-nucleo-image-default.zip" \
    "$work/bench/archives/$app.zip"
done
dist="$hubris/target/demo-stm32h753-nucleo/dist/default"
cp "$dist/final.bin" "$work/bench/final.bin"
cp "$work/bench/archives/h753.zip" "$work/bench/hubris.zip"

echo "build: Humility $humility_commit"
if [ ! -d "$work/humility" ]; then
  git clone -q https://github.com/oxidecomputer/humility.git "$work/humility"
fi
# The work directory is this script's: put the checkout back to the pinned
# commit, whatever a previous run left — untracked files too, but not
# ignored ones, which are the build cache.
git -C "$work/humility" checkout -q --force --detach "$humility_commit"
git -C "$work/humility" clean -q -fd
[ "$native" = 1 ] && rustup toolchain install 1.98.1 --profile minimal >/dev/null
# Built as tight as Cargo's profile allows, from the environment so that
# Humility's own manifest is untouched: every command is one interpreted run
# of it, so its instructions are the bench's latency. (2% fewer than its own
# release profile, measured; it catches no panics, so abort loses nothing.)
linux 'cd "$W/humility" && CARGO_PROFILE_RELEASE_LTO=fat CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 \
  CARGO_PROFILE_RELEASE_PANIC=abort CARGO_PROFILE_RELEASE_DEBUG=false \
  cargo +1.98.1 build --release -p humility-bin \
  && strip -o "$W/bench/humility" target/release/humility'

echo "build: zaqaru and zaqaru-m0"
linux 'cargo build --release --manifest-path "$Z/Cargo.toml" --target-dir "$T" -p zaqaru -p zaqaru-armv6m \
  && cp "$T/release/zaqaru-m0" "$W/bench/zaqaru-m0"'

echo "build: the bench image"
cp "$here/Dockerfile" "$here/board" "$work/bench/"
retry docker build -q --platform linux/amd64 -t zaqaru-hubris-bench "$work/bench" >/dev/null
docker save zaqaru-hubris-bench -o "$work/bench.tar"

echo "build: the module"
linux '"$T/release/zaqaru" bake "$W/bench.tar" -o "$W/hubris.wasm"'
mv "$work/hubris.wasm" "$out/hubris.wasm"
rm -f "$work/bench.tar"

# And that what was written answers: Humility, through the kernel's probe,
# listing Hubris's tasks.
node "$here/check.mjs" "$out/hubris.wasm" >/dev/null
echo "bench in $out/hubris.wasm"
echo "serve the repository and open web/?module=$(basename "$out")/hubris.wasm&live=80&terminal=80"
