#!/usr/bin/env bash
# Linux root; exercises an actual unmount while Tier holds a two-copy policy.
set -euo pipefail
tier=${TIER_BIN:-tier}
base=$(mktemp -d /tmp/tier-device.XXXXXXXX)
cleanup() {
    for dir in share a b; do
        if mountpoint -q "$base/$dir"; then umount "$base/$dir" || return 1; fi
    done
    [[ -z ${server:-} ]] || kill "$server" 2>/dev/null || true
    [[ $base == /tmp/tier-device.* ]] && rm -rf -- "$base"
}
trap cleanup EXIT
mkdir "$base"/{a,b,share}
for disk in a b; do
    truncate -s 32M "$base/$disk.img"
    mkfs.ext4 -q -F "$base/$disk.img"
    mount -o loop,nodev,nosuid "$base/$disk.img" "$base/$disk"
    "$tier" --db "$base/tier.db" target "$disk" "$base/$disk"
done
"$tier" --db "$base/tier.db" policy / '{"write_cache":[],"cache_copies":1,"persistent_storage":["a","b"],"persistent_copies":2,"persist_delay_secs":3600,"low_water":0,"high_water":1}'
"$tier" --db "$base/tier.db" serve "$base/share" --admin 127.0.0.1:0 > "$base/tier.log" 2>&1 &
server=$!
for _ in {1..100}; do mountpoint -q "$base/share" && break; sleep .1; done
mountpoint -q "$base/share" || { cat "$base/tier.log"; exit 1; }
printf 'two independent copies\n' > "$base/input"
cp "$base/input" "$base/share/saved"
umount "$base/a"
[[ $(stat -f -c %b "$base/share") == "$(stat -f -c %b "$base/b")" ]]
cmp "$base/input" "$base/share/saved"
if cp "$base/input" "$base/share/insufficient"; then exit 1; fi
[[ -z $(ls -A "$base/a") && ! -e "$base/share/insufficient" ]]
mount -o loop,nodev,nosuid "$base/a.img" "$base/a"
cp "$base/input" "$base/share/reconnected"
cmp "$base/input" "$base/share/reconnected"
