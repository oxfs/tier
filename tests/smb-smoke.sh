#!/usr/bin/env bash
# Run as root on Debian 13 with the matching tier.so installed.
set -euo pipefail
tier=$(command -v "${TIER_BIN:-tier}")
port=${SMB_PORT:-14445}
tmp=$(mktemp -d)
cleanup() {
    [[ -z ${smb_pid:-} ]] || kill -- -"$smb_pid" 2>/dev/null || true
    [[ -z ${tier_pid:-} ]] || kill "$tier_pid" 2>/dev/null || true
    if [[ ${KEEP_TEST_DATA:-0} == 1 ]]; then printf 'SMB test data: %s\n' "$tmp"; else rm -rf -- "$tmp"; fi
}
trap cleanup EXIT
mkdir -p "$tmp"/{objects,native,private,lock,state,cache,run}
chmod 755 "$tmp"
getent group tier >/dev/null || groupadd --system tier
getent passwd tier-test >/dev/null || useradd --system --no-create-home --shell /usr/sbin/nologin tier-test
tier_gid=$(getent group tier | cut -d: -f3)
"$tier" --db "$tmp/metadata" target main "$tmp/objects"
"$tier" --db "$tmp/metadata" policy / '{"write_cache":["main"],"persistent_storage":["main"],"persistent_copies":1,"persist_delay_secs":0,"low_water":99,"high_water":100}'
setpriv --regid "$tier_gid" --clear-groups "$tier" --db "$tmp/metadata" serve --socket "$tmp/tier.sock" --admin 127.0.0.1:0 > "$tmp/tier.log" 2>&1 &
tier_pid=$!
for _ in {1..100}; do [[ -S "$tmp/tier.sock" ]] && break; sleep .1; done
[[ -S "$tmp/tier.sock" ]] || { cat "$tmp/tier.log" >&2; exit 1; }
cat > "$tmp/smb.conf" <<CONF
[global]
server role = standalone server
security = user
interfaces = 127.0.0.1
bind interfaces only = yes
smb ports = $port
smb2 max read = 1048576
smb2 max write = 1048576
private dir = $tmp/private
lock directory = $tmp/lock
state directory = $tmp/state
cache directory = $tmp/cache
pid directory = $tmp/run
ncalrpc dir = $tmp/run/ncalrpc
log file = $tmp/smb.log
[tier]
path = /
vfs objects = tier
tier:socket = $tmp/tier.sock
read only = no
force group = tier
create mask = 0660
force create mode = 0660
directory mask = 0770
force directory mode = 0770
kernel share modes = no
posix locking = no
oplocks = no
level2 oplocks = no
ea support = no
store dos attributes = no
map archive = no
map hidden = no
map system = no
map readonly = no
[native]
path = $tmp/native
read only = no
force user = root
oplocks = no
level2 oplocks = no
ea support = no
store dos attributes = no
map archive = no
map hidden = no
map system = no
map readonly = no
CONF
password=tier-integration-test
printf '%s\n%s\n' "$password" "$password" | smbpasswd -s -c "$tmp/smb.conf" -a root
printf '%s\n%s\n' "$password" "$password" | smbpasswd -s -c "$tmp/smb.conf" -a tier-test
setsid smbd --foreground --no-process-group --configfile="$tmp/smb.conf" > "$tmp/smbd.log" 2>&1 &
smb_pid=$!
client=(smbclient //127.0.0.1/tier -p "$port" -U "root%$password" -s "$tmp/smb.conf")
ready=0
for _ in {1..100}; do if "${client[@]}" -c ls > "$tmp/list.log" 2>&1; then ready=1; break; fi; sleep .1; done
if [[ $ready != 1 ]]; then cat "$tmp/"{list,smb,smbd,tier}.log >&2; exit 1; fi
printf 'tier smb round trip\n' > "$tmp/input"
"${client[@]}" -c "mkdir photos; put $tmp/input photos/draft; rename photos/draft photos/final; get photos/final $tmp/output"
cmp "$tmp/input" "$tmp/output"
"${client[@]}" -c "del photos/final; rmdir photos"
if [[ ${TIER_BENCHMARK:-0} == 1 ]]; then
    SMB_TEST_PORT="$port" python3 "$(dirname "$0")/smb-benchmark.py"
fi
SMB_TEST_ROOT="$tmp" SMB_TEST_PORT="$port" TIER_TEST_PID="$tier_pid" TIER_TEST_GID="$tier_gid" TIER_BIN="$tier" \
    python3 "$(dirname "$0")/smb-integration.py"
printf 'PASS Samba VFS integration\n'
