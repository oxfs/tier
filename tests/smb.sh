#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
binary=${TIER_BIN:-"$root/target/release/tier"}
user=${TIER_USER:-root}
tmp=$(mktemp -d)
cleanup() {
    result=$?
    if [[ ${mounted:-0} == 1 ]]; then umount "$tmp/mount" || result=1; fi
    [[ -z ${pid:-} ]] || kill -- -"$pid" 2>/dev/null || true
    [[ -z ${service_pid:-} ]] || kill -- -"$service_pid" 2>/dev/null || true
    if ((result)); then cat "$tmp/"{smb,tier,process}.log >&2 || true; fi
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
# Exercise the SMB protocol directly; this does not require a privileged CIFS mount.
smb() {
    local output
    output=$("${client[@]}" -c "$1" 2>&1)
    printf '%s\n' "$output"
    [[ $output != *NT_STATUS_* ]]
}
printf 'tier SMB round trip\n' >"$tmp/input"
smb "mkdir documents; put $tmp/input documents/file.txt; ls documents/*; get documents/file.txt $tmp/before"
cmp "$tmp/input" "$tmp/before"
api scan
test ! -f "$tmp/primary/documents/file.txt"
smb "ls documents/*; get documents/file.txt $tmp/after"
cmp "$tmp/input" "$tmp/after"
printf 'rewritten after primary GC\n' >"$tmp/input"
smb "put $tmp/input documents/file.txt; get documents/file.txt $tmp/rewritten"
cmp "$tmp/input" "$tmp/rewritten"
api scan
smb "get documents/file.txt $tmp/replicated"
cmp "$tmp/input" "$tmp/replicated"
smb "rename documents/file.txt documents/renamed.txt; get documents/renamed.txt $tmp/renamed"
cmp "$tmp/input" "$tmp/renamed"
smb "rename documents renamed; ls renamed/*; get renamed/renamed.txt $tmp/moved"
cmp "$tmp/input" "$tmp/moved"
smb 'del renamed/renamed.txt; rmdir renamed'
test ! -d "$tmp/primary/documents"
test ! -d "$tmp/primary/renamed"
test ! -d "$tmp/replica/documents"
test ! -d "$tmp/replica/renamed"
if [[ ${TIER_TEST_CIFS:-0} == 1 ]]; then
    mkdir "$tmp/mount"
    mount -t cifs //127.0.0.1/tier "$tmp/mount" -o port=14447,username=root,password=test-password,vers=3.0,cache=none,actimeo=0
    mounted=1
    mkdir "$tmp/mount/mounted"
    cp "$tmp/input" "$tmp/mount/mounted/file.txt"
    cmp "$tmp/input" "$tmp/mount/mounted/file.txt"
    api scan
    test ! -f "$tmp/primary/mounted/file.txt"
    cmp "$tmp/input" "$tmp/mount/mounted/file.txt"
    mv "$tmp/mount/mounted/file.txt" "$tmp/mount/mounted/renamed.txt"
    mv "$tmp/mount/mounted" "$tmp/mount/moved"
    cmp "$tmp/input" "$tmp/mount/moved/renamed.txt"
    rm "$tmp/mount/moved/renamed.txt"
    rmdir "$tmp/mount/moved"
    umount "$tmp/mount"
    mounted=0
fi
python3 "$root/tests/detach-smb.py" "$tmp/replica"
