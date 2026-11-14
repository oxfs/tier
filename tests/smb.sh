#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
binary=${TIER_BIN:-"$root/target/release/tier"}
user=${TIER_USER:-root}
tmp=$(mktemp -d)
cleanup() {
    result=$?
    [[ -z ${pid:-} ]] || kill -- -"$pid" 2>/dev/null || true
    [[ -z ${service_pid:-} ]] || kill -- -"$service_pid" 2>/dev/null || true
    if ((result)); then cat "$tmp/smb.log" >&2; fi
    [[ ${KEEP_TEST_DATA:-0} == 1 ]] || rm -rf "$tmp"
    exit "$result"
}
trap cleanup EXIT
mkdir -p "$tmp"/{primary,replica,private,lock,state,cache,run}
chmod 755 "$tmp"
chown "$user" "$tmp/primary" "$tmp/replica"
cat >"$tmp/config.toml" <<EOF
primary = "$tmp/primary"
[replicas]
backup = "$tmp/replica"
[[backup]]
path = ""
count = 1
gc = true
EOF
setsid runuser -u "$user" -- env TIER_WEB_LISTEN=127.0.0.1:14448 "$binary" "$tmp/config.toml" >"$tmp/tier.log" 2>&1 &
service_pid=$!
api() {
    python3 - "$1" <<'PY'
import json
import sys
import urllib.request
request = urllib.request.Request('http://127.0.0.1:14448/api/' + sys.argv[1], data=b'{}', headers={'X-Tier-Request': '1', 'Content-Type': 'application/json'})
with urllib.request.urlopen(request, timeout=60) as response:
    result = json.load(response)
    assert not result.get('errors'), result
PY
}
for _ in {1..50}; do
    if api init 2>/dev/null; then break; fi
    sleep 0.1
done
test -f "$tmp/primary/.tier/storage.json"
cat >"$tmp/smb.conf" <<EOF
[global]
server role = standalone server
interfaces = 127.0.0.1
bind interfaces only = yes
smb ports = 14447
private dir = $tmp/private
lock directory = $tmp/lock
state directory = $tmp/state
cache directory = $tmp/cache
pid directory = $tmp/run
ncalrpc dir = $tmp/run/ncalrpc
log file = $tmp/smb.log
log level = 3
[tier]
path = $tmp/primary
vfs objects = tier
tier:config = $tmp/config.toml
read only = no
force user = $user
kernel share modes = no
posix locking = no
store dos attributes = no
ea support = no
map archive = no
EOF
printf 'test-password\ntest-password\n' | smbpasswd -s -c "$tmp/smb.conf" -a root
setsid smbd -F --no-process-group -s "$tmp/smb.conf" >"$tmp/process.log" 2>&1 &
pid=$!
sleep 1
client=(smbclient //127.0.0.1/tier -p 14447 -U root%test-password)
printf 'tier SMB round trip\n' >"$tmp/input"
"${client[@]}" -c "mkdir documents; put $tmp/input documents/file.txt; ls documents/*; get documents/file.txt $tmp/before"
cmp "$tmp/input" "$tmp/before"
api scan
test ! -f "$tmp/primary/documents/file.txt"
"${client[@]}" -c "ls documents/*; get documents/file.txt $tmp/after"
cmp "$tmp/input" "$tmp/after"
printf 'rewritten after primary GC\n' >"$tmp/input"
"${client[@]}" -c "put $tmp/input documents/file.txt; get documents/file.txt $tmp/rewritten"
cmp "$tmp/input" "$tmp/rewritten"
api scan
"${client[@]}" -c "get documents/file.txt $tmp/replicated"
cmp "$tmp/input" "$tmp/replicated"
"${client[@]}" -c 'rename documents/file.txt documents/renamed.txt; del documents/renamed.txt; rmdir documents'
test ! -d "$tmp/primary/documents"
python3 "$root/tests/detach-smb.py" "$tmp/replica"
