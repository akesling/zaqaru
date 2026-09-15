#!/usr/bin/env bash
# Run from any directory; Docker is confined to the existing experiment wrapper.
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.."
case ${1:-help} in
  prepare)
    shift
    exec python3 tools/evolution/prepare-sources.py "$@"
    ;;
  build)
    shift
    exec tools/microbench/experiment.sh exec bash tools/evolution/build-linux.sh "$@"
    ;;
  node) shift; exec node tools/evolution/test-node.mjs "$@" ;;
  browser) shift; exec node tools/evolution/test-browser.mjs "$@" ;;
  capture) shift; exec node tools/evolution/capture-node.mjs "$@" ;;
  warm-node) shift; exec node tools/evolution/warm-node.mjs "$@" ;;
  warm-browser) shift; exec node tools/evolution/test-browser.mjs --warm "$@" ;;
  profile) shift; exec tools/microbench/experiment.sh exec target/release/zaqaru-evolution profile "$@" ;;
  *) echo 'Usage: bash tools/evolution/run.sh {prepare [STRUCTFS_REPO]|build [KERNEL SCALE [FEATURES]]|node [REGION_MEMBERS]|browser [REGION_MEMBERS]}'
     echo 'Additional commands: capture TEMPLATE OUTPUT_DIR [REGION_MEMBERS] [BASELINE], warm-node OUTPUT.json MANIFEST..., warm-browser OUTPUT.json MANIFEST..., profile MODULE [FUNCTION_INDEX]'
     echo 'FEATURES: evolution (default), regions, guarded-stack, stack-forwarding, virtual-flags, shared-exit, direct-transfers; experiments may be comma-separated.' ;;
esac
