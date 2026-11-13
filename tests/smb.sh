#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
binary=${TIER_BIN:-"$root/target/release/tier"}
user=${TIER_USER:-root}
tmp=$(mktemp -d)
cleanup() {
    result=$?
    [[ -z ${pid:-} ]] || kill -- -"$pid" 2>/dev/null || true
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
runuser -u "$user" -- "$binary" "$tmp/config.toml" init
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
runuser -u "$user" -- "$binary" "$tmp/config.toml" sync documents/file.txt
test ! -f "$tmp/primary/documents/file.txt"
"${client[@]}" -c "ls documents/*; get documents/file.txt $tmp/after"
cmp "$tmp/input" "$tmp/after"
printf 'rewritten after primary GC\n' >"$tmp/input"
"${client[@]}" -c "put $tmp/input documents/file.txt; get documents/file.txt $tmp/rewritten"
cmp "$tmp/input" "$tmp/rewritten"
runuser -u "$user" -- "$binary" "$tmp/config.toml" sync documents/file.txt
"${client[@]}" -c "get documents/file.txt $tmp/replicated"
cmp "$tmp/input" "$tmp/replicated"
"${client[@]}" -c 'rename documents/file.txt documents/renamed.txt; del documents/renamed.txt; rmdir documents'
test ! -d "$tmp/primary/documents"
