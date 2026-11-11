#!/usr/bin/env bash
# Run TLC and ensure removing each safety fence yields an actual counterexample.
set -euo pipefail
jar=$(realpath "${TLA2TOOLS_JAR:?set TLA2TOOLS_JAR to tla2tools.jar}")
model=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"
cp "$model/"*.tla "$model/"*.cfg .
run() {
    java -XX:+UseParallelGC -Xmx1g -cp "$jar" tlc2.TLC -workers 2 \
        -metadir "$work/states" -config "$2" "$1.tla"
}
counterexample() {
    local name=$1 guard=$2 invariant=$3
    sed "s/$guard = TRUE/$guard = FALSE/" "$model/$name.cfg" >"$work/unsafe.cfg"
    if run "$name" unsafe.cfg >"$work/counterexample.log" 2>&1; then
        echo "Missing counterexample for $name/$guard" >&2
        exit 1
    fi
    if ! grep -Eq "Invariant $invariant is violated" "$work/counterexample.log"; then
        cat "$work/counterexample.log" >&2
        exit 1
    fi
    printf 'PASS counterexample: %s/%s\n' "$name" "$guard"
}
for name in Protection Durability PhysicalRename Placement DirectAccess; do
    run "$name" "$name.cfg"
done
counterexample Protection CheckGeneration '(PublishesCurrent|DataAvailable)'
counterexample Protection CheckVerification VerifiedCopiesHaveData
counterexample Protection CheckRetention DataAvailable
counterexample Protection CheckRead ReadsCurrent
counterexample Durability CheckInvalidation ProtectedContentUnchanged
counterexample Durability CheckDataSync AcknowledgedDurable
counterexample Durability CheckMetadataSync AcknowledgedDurable
counterexample PhysicalRename CheckLinkSync DurableMappingHasBody
counterexample PhysicalRename CheckMappingSync DurableMappingHasBody
counterexample PhysicalRename CheckGCCurrent '(DurableMappingHasBody|LiveMappingHasBody)'
counterexample PhysicalRename CheckIntent NoUntrackedAlias
counterexample Placement CheckPolicy PublishesCurrentPolicy
counterexample Placement CheckLive PublishesLiveNode
counterexample DirectAccess CheckPublication GrantsOutsideMutation
counterexample DirectAccess CheckEpoch '(ReadsCurrent|HandlesProtected)'
counterexample DirectAccess CheckPins HandlesProtected
counterexample DirectAccess CheckDeadline '(ReadsCurrent|HandlesProtected)'
counterexample DirectAccess CheckDaemon ReadsCurrent
counterexample DirectAccess CheckDisconnect '(ReadsCurrent|HandlesProtected)'
