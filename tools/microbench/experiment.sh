#!/usr/bin/env bash
# Repeatable local experiments; all Docker invocations live in linux.sh.
set -euo pipefail
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
action=${1:-help}
if [[ $# -gt 0 ]]; then shift; fi
case "$action" in
    save)
        name=${1:?Usage: experiment.sh save NAME}
        if [[ ! $name =~ ^[a-zA-Z0-9_-]+$ ]]; then
            echo "Use letters, digits, underscores or hyphens for the executable name" >&2
            exit 1
        fi
        exec "$SCRIPT_DIR/linux.sh" --exec bash -c \
            'cargo build --locked --release -p zaqaru && cp target/release/zaqaru "$1"' \
            _ "/work/benchmark-results/$name.zaqaru"
        ;;
    ab)
        exec "$SCRIPT_DIR/linux.sh" --binary /work/benchmark-results/candidate.zaqaru \
            --against /work/benchmark-results/baseline.zaqaru --modes bytecode \
            --output /work/benchmark-results/ab.json "$@"
        ;;
    baseline|candidate)
        exec "$SCRIPT_DIR/linux.sh" --output "/work/benchmark-results/$action.json" "$@"
        ;;
    test)
        exec "$SCRIPT_DIR/linux.sh" --exec cargo test --locked --release \
            -p zaqaru-cpu --lib --test bytecode "$@"
        ;;
    compare)
        exec python3 "$SCRIPT_DIR/compare.py" \
            "$SCRIPT_DIR/../../benchmark-results/baseline.json" \
            "$SCRIPT_DIR/../../benchmark-results/candidate.json" "$@"
        ;;
    exec)
        exec "$SCRIPT_DIR/linux.sh" --exec "$@"
        ;;
    *)
        echo "Usage: $0 {baseline|candidate} [measure.py options]"
        echo "       $0 test [cargo test options]"
        echo "       $0 compare [--threshold FRACTION]"
        echo "       $0 save NAME | ab [measure.py options]"
        echo "       $0 exec COMMAND [ARG ...]"
        [[ $action == help ]]
        ;;
esac
