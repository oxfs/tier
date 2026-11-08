#!/usr/bin/env bash
# Run as root on Linux with tier, smbd, smbclient and FUSE installed.
set -euo pipefail
tier=${TIER_BIN:-tier}
port=${SMB_PORT:-14445}
tmp=$(mktemp -d)
mkdir -p "$tmp"/{mnt,objects,private,lock,state,cache,run}
cleanup() {
    [[ -z ${smb_pid:-} ]] || kill "$smb_pid" 2>/dev/null || true
    mountpoint -q "$tmp/mnt" && umount "$tmp/mnt" || true
    [[ -z ${tier_pid:-} ]] || kill "$tier_pid" 2>/dev/null || true
    [[ -d "$tmp" && "$tmp" == /tmp/tmp.* ]] && rm -rf -- "$tmp"
}
trap cleanup EXIT
"$tier" --db "$tmp/tier.db" target main "$tmp/objects"
"$tier" --db "$tmp/tier.db" policy / '{"front":["main"],"front_count":1,"long":["main"],"long_count":1,"delay_secs":0}'
"$tier" --db "$tmp/tier.db" serve "$tmp/mnt" --admin 127.0.0.1:0 > "$tmp/tier.log" 2>&1 &
tier_pid=$!
for _ in {1..100}; do mountpoint -q "$tmp/mnt" && break; sleep .1; done
mountpoint -q "$tmp/mnt" || { cat "$tmp/tier.log" >&2; exit 1; }
python3 - "$tmp/mnt" <<'PY'
import errno
import os
import sqlite3
import sys
from pathlib import Path
root = Path(sys.argv[1])
path = root / "lock-test"
with path.open("wb") as writer:
    writer.write(b"first")
    writer.flush()
    os.fsync(writer.fileno())
    try:
        os.open(path, os.O_RDWR)
    except OSError as exc:
        assert exc.errno == errno.EBUSY, exc
    else:
        raise AssertionError("second writer unexpectedly opened")
with path.open("rb") as reader:
    replacement = root / "replacement"
    replacement.write_bytes(b"second")
    replacement.replace(path)
    assert reader.read() == b"first"
assert path.read_bytes() == b"second"
path.unlink()
for replace in (False, True):
    path = root / "detach-test"
    path.write_bytes(b"original")
    with path.open("r+b", buffering=0) as writer:
        writer.write(b"modified")
        try:
            os.truncate(path, 2)
        except OSError as error:
            assert error.errno == errno.EBUSY, error
        else:
            raise AssertionError("path truncation bypassed the writer")
        if replace:
            other = root / "replacement"
            other.write_bytes(b"new")
            other.replace(path)
        else:
            path.unlink()
        try:
            writer.write(b"lost")
        except OSError as error:
            assert error.errno == errno.ESTALE, error
        else:
            raise AssertionError("detached handle acknowledged a write")
    with sqlite3.connect(root.parent / "tier.db") as db:
        key, = db.execute("SELECT c.object FROM trash t JOIN copies c ON c.node=t.node WHERE original='/detach-test' ORDER BY t.id DESC LIMIT 1").fetchone()
    assert (root.parent / "objects" / key).read_bytes() == b"modified"
    if replace:
        assert path.read_bytes() == b"new"
PY
chmod 755 "$tmp"
cat > "$tmp/smb.conf" <<EOF
[global]
server role = standalone server
security = user
map to guest = bad user
interfaces = 127.0.0.1
bind interfaces only = yes
smb ports = $port
private dir = $tmp/private
lock directory = $tmp/lock
state directory = $tmp/state
cache directory = $tmp/cache
pid directory = $tmp/run
ncalrpc dir = $tmp/run/ncalrpc
log file = $tmp/smb.log
EOF
sed -e "s|/srv/tier/share|$tmp/mnt|" -e "s/force group = tier/force group = root/" -e "s/guest ok = no/guest ok = yes/" deploy/samba-share.conf >> "$tmp/smb.conf"
printf '    force user = root\n' >> "$tmp/smb.conf"
setsid smbd --foreground --no-process-group --configfile="$tmp/smb.conf" > "$tmp/smbd.log" 2>&1 &
smb_pid=$!
client=(smbclient //127.0.0.1/tier -p "$port" -U 'tierguest%' -s "$tmp/smb.conf")
for _ in {1..100}; do "${client[@]}" -c ls >/dev/null 2>&1 && break; sleep .1; done
printf 'tier smb round trip\n' > "$tmp/input"
"${client[@]}" -c "mkdir photos; put $tmp/input photos/draft; rename photos/draft photos/final; get photos/final $tmp/output"
cmp "$tmp/input" "$tmp/output"
"${client[@]}" -c "del photos/final; rmdir photos"
[[ ! -e "$tmp/mnt/photos" ]]
