#!/bin/sh
# Gathers the source files a Hubris build's DWARF names, for a debugger to
# show the code: each one copied to <out-dir> at the path the DWARF gives,
# which is the path Hubris's build remapped it to (`remap_paths` in
# build/xtask/src/dist.rs) — so read back from where it came from:
#
#   /hubris/build/…    what build scripts generated (Idol's stubs, the
#                      I2C configuration), in the checkout's target
#                      directory, under whichever target it was built for
#   /hubris/…          the Hubris checkout
#   /crates.io/…       Cargo's registry sources
#   /git/…             Cargo's git checkouts
#   /rustc/HASH/…      the standard library at that compiler's commit, from
#                      rust-lang/rust on GitHub, which is where a toolchain
#                      without its rust-src component has it
#
#   demo/hubris/sources.sh <hubris-checkout> <list> <out-dir>
#
# <list> is what `zaqaru-m0 sources ARCHIVE` prints. A file that is nowhere
# to be found is left out, and said.
set -eu
hubris=$(cd "$1" && pwd)
list=$2
out=$3
cargo_home=${CARGO_HOME:-$HOME/.cargo}
registry=$(ls -d "$cargo_home"/registry/src/index.crates.io-* 2>/dev/null | head -1)
mkdir -p "$out"
found=0 missing=0
while IFS= read -r path; do
  case "$path" in
    /hubris/build/*)
      from=
      for dir in "$hubris"/target/*/release/build; do
        [ -f "$dir/${path#/hubris/build/}" ] && from="$dir/${path#/hubris/build/}" && break
      done
      [ -n "$from" ] || from=/nonexistent ;;
    /hubris/*) from="$hubris/${path#/hubris/}" ;;
    /crates.io/*) from="$registry/${path#/crates.io/}" ;;
    /git/*) from="$cargo_home/git/checkouts/${path#/git/}" ;;
    /rustc/*) from= ;;
    *) continue ;;
  esac
  to="$out$path"
  [ -f "$to" ] && { found=$((found + 1)); continue; }
  mkdir -p "$(dirname "$to")"
  if [ -n "$from" ] && [ -f "$from" ]; then
    cp "$from" "$to"
  elif [ -z "$from" ]; then
    rest=${path#/rustc/}
    commit=${rest%%/*}
    curl -fsSL --retry 2 "https://raw.githubusercontent.com/rust-lang/rust/$commit/${rest#*/}" -o "$to" 2>/dev/null || rm -f "$to"
  fi
  if [ -f "$to" ]; then found=$((found + 1)); else missing=$((missing + 1)); echo "sources: not found: $path" >&2; fi
done < "$list"
echo "sources: $found files in $out, $missing not found"
