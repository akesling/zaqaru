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
#   task/doom/…        relative: C a task built with the cc crate, whose
#                      DWARF joins the directory it was compiled in to a
#                      path that already has it — the first file of the
#                      checkout's that the path, or a tail of it, names —
#                      and the rest of the C and headers beside it, which
#                      its line tables need not name but its code does
#
# <out-dir>/.files lists what was kept, a path a line, as the DWARF would
# name each: for the board's index of the code to read them all.
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
kept="$out/.files.new"
beside="$out/.beside"
: > "$kept"
: > "$beside"
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
    /*) continue ;;
    ./*|*/./*|*/../*) continue ;;
    *)
      from=/nonexistent
      rest=$path
      while [ -n "$rest" ]; do
        [ -f "$hubris/$rest" ] && from="$hubris/$rest" && break
        case "$rest" in */*) rest=${rest#*/} ;; *) rest= ;; esac
      done
      [ -f "$from" ] && printf '%s\t%s\n' "$(dirname "$from")" "$(dirname "$path")" >> "$beside" ;;
  esac
  to="$out/${path#/}"
  [ -f "$to" ] && { found=$((found + 1)); echo "$path" >> "$kept"; continue; }
  mkdir -p "$(dirname "$to")"
  if [ -n "$from" ] && [ -f "$from" ]; then
    cp "$from" "$to"
  elif [ -z "$from" ]; then
    rest=${path#/rustc/}
    commit=${rest%%/*}
    curl -fsSL --retry 2 "https://raw.githubusercontent.com/rust-lang/rust/$commit/${rest#*/}" -o "$to" 2>/dev/null || rm -f "$to"
  fi
  if [ -f "$to" ]; then found=$((found + 1)); echo "$path" >> "$kept"; else missing=$((missing + 1)); echo "sources: not found: $path" >&2; fi
done < "$list"
# The C and headers beside a relative path's file, named as it is.
sort -u "$beside" | while IFS="$(printf '\t')" read -r from named; do
  for file in "$from"/*.c "$from"/*.h; do
    [ -f "$file" ] || continue
    path="$named/$(basename "$file")"
    [ -f "$out/$path" ] || { mkdir -p "$out/$named" && cp "$file" "$out/$path"; }
    echo "$path" >> "$kept"
  done
done
sort -u "$kept" > "$out/.files"
rm -f "$kept" "$beside"
echo "sources: $found files in $out, $missing not found; $(wc -l < "$out/.files" | tr -d ' ') kept in all"
