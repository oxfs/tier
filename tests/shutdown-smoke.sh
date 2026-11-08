#!/usr/bin/env bash
set -euo pipefail
tier=${TIER_BIN:-tier}
dir=$(mktemp -d /tmp/tier-ns.XXXXXXXX)
mkdir "$dir"/{share,other}
cleanup(){
    [[ -z ${clone:-} ]] || kill "$clone" 2>/dev/null || true
    [[ -z ${daemon:-} ]] || kill "$daemon" 2>/dev/null || true
    [[ -z ${other:-} ]] || kill "$other" 2>/dev/null || true
    for mount in "$dir/share" "$dir/other"; do if mountpoint -q "$mount"; then umount "$mount"; fi; done
    rm -rf -- "$dir"
}
trap cleanup EXIT
"$tier" --db "$dir/db" serve "$dir/share" --admin 127.0.0.1:0 >"$dir/log" 2>&1 & daemon=$!
"$tier" --db "$dir/db2" serve "$dir/other" --admin 127.0.0.1:0 >"$dir/log2" 2>&1 & other=$!
for _ in {1..80};do mountpoint -q "$dir/share" && mountpoint -q "$dir/other" && break; sleep .1; done
ls "$dir/share" "$dir/other" >/dev/null
unshare --mount --propagation private sleep 90 & clone=$!
for _ in {1..80};do [[ $(readlink /proc/$clone/ns/mnt) != "$(readlink /proc/self/ns/mnt)" ]] && break; sleep .1; done
nsenter -t "$clone" -m -- ls "$dir/share" >/dev/null
kill -TERM "$daemon"
timeout 8 tail --pid="$daemon" -f /dev/null
wait "$daemon"; daemon=
if mountpoint -q "$dir/share"; then exit 1; fi
nsenter -t "$clone" -m -- findmnt --mountpoint "$dir/share" >/dev/null
result=0
nsenter -t "$clone" -m -- timeout 2 ls "$dir/share" >/dev/null 2>&1 || result=$?
[[ $result -eq 2 ]]
ls "$dir/other" >/dev/null
kill "$clone"; wait "$clone" || true; clone=
kill -TERM "$other"; timeout 8 tail --pid="$other" -f /dev/null; wait "$other"; other=
if grep -q "failed to unmount" "$dir/log" "$dir/log2"; then cat "$dir/log" "$dir/log2"; exit 1; fi
echo 'PASS cloned mount remains disconnected, sibling connection stays operational, ordinary and cloned stops both exit cleanly'
