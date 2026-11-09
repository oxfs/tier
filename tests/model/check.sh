#!/usr/bin/env bash
# Run TLC and ensure removing each safety fence yields an actual counterexample.
set -euo pipefail
jar=$(realpath "${TLA2TOOLS_JAR:?set TLA2TOOLS_JAR to tla2tools.jar}")
model=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"
cp "$model/Protection.tla" "$model/Protection.cfg" .
run() {
    java -XX:+UseParallelGC -Xmx1g -cp "$jar" tlc2.TLC -workers 2 \
        -metadir "$work/states" -config "$1" Protection.tla
}
run Protection.cfg
for guard in CheckGeneration CheckVerification CheckRetention CheckRead; do
    case "$guard" in
        CheckGeneration) invariant='(PublishesCurrent|DataAvailable)' ;;
        CheckVerification) invariant='VerifiedCopiesHaveData' ;;
        CheckRetention) invariant='DataAvailable' ;;
        CheckRead) invariant='ReadsCurrent' ;;
    esac
    sed "s/$guard = TRUE/$guard = FALSE/" "$model/Protection.cfg" >"$work/unsafe.cfg"
    if run unsafe.cfg >"$work/counterexample.log" 2>&1; then
        echo "Missing counterexample for $guard" >&2
        exit 1
    fi
    if ! grep -Eq "Invariant $invariant is violated" "$work/counterexample.log"; then
        cat "$work/counterexample.log" >&2
        exit 1
    fi
    printf 'PASS counterexample: %s\n' "$guard"
done
