#!/bin/sh
# Makes the Hubris bench: Hubris's STM32G031 demo built, a container that
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
hubris=$(cd "${1:?usage: demo/hubris/build.sh <hubris-checkout> [out-dir]}" && pwd)
out=${2:-$repo/web/demo}
mkdir -p "$out"
out=$(cd "$out" && pwd)
work="$out/hubris-build"
mkdir -p "$work/bench"

# Humility at a known commit, with the GDB probe the bench needs on top.
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

echo "build: Hubris, app-g031.toml"
(cd "$hubris" && cargo xtask dist app/demo-stm32g0-nucleo/app-g031.toml)
dist="$hubris/target/demo-stm32g031-nucleo/dist/default"
cp "$dist/final.bin" "$work/bench/final.bin"
cp "$dist/build-demo-stm32g031-nucleo-image-default.zip" "$work/bench/hubris.zip"

echo "build: Humility $humility_commit, with the GDB probe"
if [ ! -d "$work/humility" ]; then
  git clone -q https://github.com/oxidecomputer/humility.git "$work/humility"
fi
# The work directory is this script's: put the checkout back to the pinned
# commit and the patch on it, whatever a previous run left.
git -C "$work/humility" checkout -q --force --detach "$humility_commit"
# Untracked files too — the patch adds one — but not ignored ones, which
# are the build cache.
git -C "$work/humility" clean -q -fd
git -C "$work/humility" apply "$here/humility-gdb-probe.patch"
[ "$native" = 1 ] && rustup toolchain install 1.98.1 --profile minimal >/dev/null
linux 'cd "$W/humility" && cargo +1.98.1 build --release --no-default-features -p humility-bin \
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
